"""FAC-07 verifier-mutation guard tests.

Each test builds a throwaway git repo under pytest's tmp_path. The upstream oracle manifest,
selftest and evidence loaders are monkeypatched at their module attributes (the guard calls
them through module attributes on purpose), so these tests do not need the PG18 container.
"""

from __future__ import annotations

import json
import subprocess
from pathlib import Path
from typing import Any

import pytest

from pgarbor_factory import evidence as factory_evidence
from pgarbor_factory import guard
from pgarbor_factory.records import EvidencePack, FamilyEvidence
from pgarbor_harness.oracle import manifest as harness_manifest
from pgarbor_harness.oracle.manifest import ManifestCheck, TreeChange
from pgarbor_harness.selftest import run as harness_selftest
from pgarbor_harness.selftest.run import SelftestCheck, SelftestReport

REFERENCE = {
    "image_digest": "sha256:74935e72241653ca55e0414067e6d8763aceb8a810eb51b452253ec3dcfc4336",
    "server_version": "18.6 (Debian 18.6-1.pgdg13+2)",
    "fingerprint_sha256": "00" * 32,
}
EXPECTED_REL = "tests/expected/smoke/a.json"
CASE_REL = "tests/cases/smoke/a.yaml"


def _git(repo: Path, *args: str) -> None:
    subprocess.run(
        ["git", "-c", "user.name=t", "-c", "user.email=t@example.invalid", *args],
        cwd=str(repo), check=True, capture_output=True, text=True,
    )


def _write(repo: Path, rel: str, text: str) -> None:
    p = repo / rel
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(text, encoding="utf-8")


def _write_manifest(repo: Path) -> None:
    doc = {"format": "pgarbor-oracle-manifest/1", "reference": REFERENCE}
    _write(repo, "runs/state/oracle_manifest.json", json.dumps(doc))


@pytest.fixture
def repo(tmp_path: Path) -> Path:
    root = tmp_path / "repo"
    root.mkdir()
    _git(root, "init", "-q")
    _write(root, "contracts/ARCHITECTURE.md", "# arch\n")
    _write(root, EXPECTED_REL, '{"x": 1}\n')
    _write(root, CASE_REL, "case_id: smoke.a\nsetup: select 1\n")
    _write(root, "crates/pgarbor-core/src/lib.rs", "pub fn f() {}\n")
    _git(root, "add", "-A")
    _git(root, "commit", "-q", "-m", "base")
    _write_manifest(root)
    return root


@pytest.fixture
def stubs(monkeypatch: pytest.MonkeyPatch) -> dict[str, Any]:
    """Default: manifest clean, no previous campaign, selftest ok. Tests override entries."""
    state: dict[str, Any] = {
        "check": ManifestCheck(ok=True),
        "last": None,
        "selftest": SelftestReport(ok=True),
        "freeze_calls": [],
    }

    def fake_check(paths: Any) -> ManifestCheck:
        return state["check"]

    def fake_freeze(paths: Any, reference: dict[str, str]) -> dict[str, Any]:
        state["freeze_calls"].append(reference)
        return {}

    def fake_run_offline(fixtures: Any = None) -> SelftestReport:
        return state["selftest"]

    monkeypatch.delenv("PGARBOR_ORACLE_REVIEW", raising=False)
    monkeypatch.setattr(harness_manifest, "check", fake_check)
    monkeypatch.setattr(harness_manifest, "freeze", fake_freeze)
    monkeypatch.setattr(harness_selftest, "run_offline", fake_run_offline)
    monkeypatch.setattr(factory_evidence, "load_last", lambda root: state["last"])
    return state


def _pack(families: list[FamilyEvidence], finished: int = 1000) -> EvidencePack:
    return EvidencePack(
        campaign_id="20260101T000000Z-abcdef12", epoch=0, mode="cathedral",
        git={"sha": "a" * 40, "branch": "main", "dirty": False},
        toolchain={"rustc": "rustc 1.0", "cargo": "cargo 1.0"},
        platform={"os": "Linux", "kernel": "k", "arch": "x86_64", "python": "3.12"},
        reference=dict(REFERENCE), corpus={}, seeds=[1], started_unix=900,
        finished_unix=finished, build={"ok": True}, families=families, totals={},
        critical={"corruption": 0, "crash": 0},
    )


def _kinds(report: guard.GuardReport) -> list[str]:
    return [v.kind for v in report.violations]


def _change(tree: str, path: str, change: str = "modified") -> ManifestCheck:
    return ManifestCheck(ok=False, changes=[TreeChange(tree, path, change)], message="changed")


def _commit_all(repo: Path, msg: str) -> None:
    _git(repo, "add", "-A")
    _git(repo, "commit", "-q", "-m", msg)


def test_clean_tree_passes(repo: Path, stubs: dict[str, Any]) -> None:
    report = guard.check_campaign_preconditions(repo)
    assert report.ok is True
    assert report.violations == []


def test_unreviewed_expected_edit_refuses(repo: Path, stubs: dict[str, Any]) -> None:
    _write(repo, EXPECTED_REL, '{"x": 2}\n')
    _commit_all(repo, "edit expected")
    stubs["check"] = _change("expected", EXPECTED_REL)
    report = guard.check_campaign_preconditions(repo)
    assert report.ok is False
    assert any(v.kind == "unreviewed_change" and v.path == EXPECTED_REL
               for v in report.violations)


def test_reviewed_expected_edit_passes(repo: Path, stubs: dict[str, Any],
                                       monkeypatch: pytest.MonkeyPatch) -> None:
    _write(repo, EXPECTED_REL, '{"x": 2}\n')
    _commit_all(repo, "edit expected")
    stubs["check"] = _change("expected", EXPECTED_REL)

    monkeypatch.setenv("PGARBOR_ORACLE_REVIEW", "1")
    guard.record_review(repo, "reviewer", [EXPECTED_REL], "re-recorded from PG18.6")
    monkeypatch.delenv("PGARBOR_ORACLE_REVIEW")

    report = guard.check_campaign_preconditions(repo)
    assert report.ok is True, report.violations
    assert stubs["freeze_calls"] == [REFERENCE]


def test_protected_change_appearing_in_two_trees_is_reported_once(
        repo: Path, stubs: dict[str, Any]) -> None:
    _write(repo, EXPECTED_REL, '{"x": 3}\n')
    _commit_all(repo, "edit expected")
    stubs["check"] = ManifestCheck(ok=False, changes=[
        TreeChange("protected", EXPECTED_REL, "modified"),
        TreeChange("expected", EXPECTED_REL, "modified"),
    ])
    report = guard.check_campaign_preconditions(repo)
    unreviewed = [v for v in report.violations if v.kind == "unreviewed_change"]
    assert len(unreviewed) == 1


def test_review_with_unknown_sha_does_not_cover(repo: Path, stubs: dict[str, Any]) -> None:
    _write(repo, EXPECTED_REL, '{"x": 4}\n')
    _commit_all(repo, "edit expected")
    stubs["check"] = _change("expected", EXPECTED_REL)
    record = {"at_unix": 2000, "reviewer": "r", "git_sha": "0123456789abcdef0123456789abcdef01234567",
              "paths": [EXPECTED_REL], "reason": "bogus sha"}
    _write(repo, guard.REVIEWS_REL, json.dumps(record) + "\n")
    report = guard.check_campaign_preconditions(repo)
    assert report.ok is False
    assert "unreviewed_change" in _kinds(report)


def test_env_var_does_not_flip_a_violation(repo: Path, stubs: dict[str, Any],
                                           monkeypatch: pytest.MonkeyPatch) -> None:
    _write(repo, EXPECTED_REL, '{"x": 5}\n')
    _commit_all(repo, "edit expected")
    stubs["check"] = _change("expected", EXPECTED_REL)
    monkeypatch.setenv("PGARBOR_ORACLE_REVIEW", "1")
    report = guard.check_campaign_preconditions(repo)
    assert report.ok is False


def test_run_selftest_false_is_rejected(repo: Path, stubs: dict[str, Any]) -> None:
    with pytest.raises(ValueError):
        guard.check_campaign_preconditions(repo, run_selftest=False)


def test_malformed_review_line_is_a_violation(repo: Path, stubs: dict[str, Any]) -> None:
    _write(repo, guard.REVIEWS_REL, "not json\n")
    report = guard.check_campaign_preconditions(repo)
    assert report.ok is False
    assert "review_record_invalid" in _kinds(report)


def test_manifest_exception_fails_closed(repo: Path, stubs: dict[str, Any],
                                         monkeypatch: pytest.MonkeyPatch) -> None:
    def boom(paths: Any) -> ManifestCheck:
        raise NotImplementedError("HAR-05: check manifest")

    monkeypatch.setattr(harness_manifest, "check", boom)
    report = guard.check_campaign_preconditions(repo)
    assert report.ok is False
    assert "manifest_check_failed" in _kinds(report)


def test_reference_change_is_always_a_violation(repo: Path, stubs: dict[str, Any]) -> None:
    stubs["check"] = ManifestCheck(ok=False, reference_changed=True, message="image digest")
    report = guard.check_campaign_preconditions(repo)
    assert report.ok is False
    assert "reference_changed" in _kinds(report)


def test_case_count_drop_is_denominator_shrink(repo: Path, stubs: dict[str, Any]) -> None:
    stubs["last"] = _pack([FamilyEvidence(name="differential", status="RAN", total_cataloged=5)])
    report = guard.check_campaign_preconditions(repo)
    assert report.ok is False
    assert "denominator_shrank" in _kinds(report)


def test_case_count_drop_with_covering_review_passes(repo: Path, stubs: dict[str, Any],
                                                     monkeypatch: pytest.MonkeyPatch) -> None:
    stubs["last"] = _pack([FamilyEvidence(name="differential", status="RAN", total_cataloged=5)],
                          finished=1000)
    monkeypatch.setenv("PGARBOR_ORACLE_REVIEW", "1")
    guard.record_review(repo, "reviewer", ["denominator:differential"],
                        "two duplicate cases merged")
    monkeypatch.delenv("PGARBOR_ORACLE_REVIEW")
    report = guard.check_campaign_preconditions(repo)
    assert report.ok is True, report.violations


def test_unverifiable_family_is_refused(repo: Path, stubs: dict[str, Any]) -> None:
    stubs["last"] = _pack([FamilyEvidence(name="wire", status="RAN", total_cataloged=3)])
    report = guard.check_campaign_preconditions(repo)
    assert report.ok is False
    assert "denominator_unverifiable" in _kinds(report)


def test_added_quarantine_is_refused(repo: Path, stubs: dict[str, Any]) -> None:
    _write(repo, CASE_REL, "case_id: smoke.a\nsetup: select 1\nquarantine:\n  reason: flaky\n")
    _commit_all(repo, "quarantine case")
    stubs["check"] = _change("cases", CASE_REL)
    report = guard.check_campaign_preconditions(repo)
    assert report.ok is False
    assert "unreviewed_change" in _kinds(report)
    assert "quarantine_unreviewed" in _kinds(report)


def test_nested_quarantine_text_is_not_a_top_level_key(repo: Path, stubs: dict[str, Any]) -> None:
    _write(repo, CASE_REL, "case_id: smoke.a\nnotes: |\n  quarantine: no\n")
    _commit_all(repo, "notes only")
    stubs["check"] = _change("cases", CASE_REL)
    report = guard.check_campaign_preconditions(repo)
    assert "quarantine_unreviewed" not in _kinds(report)


def test_selftest_failure_refuses(repo: Path, stubs: dict[str, Any]) -> None:
    stubs["selftest"] = SelftestReport(ok=False, checks=[SelftestCheck("identity", False, "x")])
    report = guard.check_campaign_preconditions(repo)
    assert report.ok is False
    assert "selftest_failed" in _kinds(report)


def test_selftest_exception_refuses(repo: Path, stubs: dict[str, Any],
                                    monkeypatch: pytest.MonkeyPatch) -> None:
    def boom(fixtures: Any = None) -> SelftestReport:
        raise NotImplementedError("HAR-28: offline selftest")

    monkeypatch.setattr(harness_selftest, "run_offline", boom)
    report = guard.check_campaign_preconditions(repo)
    assert report.ok is False
    assert "selftest_failed" in _kinds(report)


def test_record_review_requires_review_env(repo: Path, stubs: dict[str, Any]) -> None:
    with pytest.raises(PermissionError):
        guard.record_review(repo, "reviewer", [EXPECTED_REL], "no env")
    assert not (repo / guard.REVIEWS_REL).exists()
    assert stubs["freeze_calls"] == []


def test_record_review_rejects_escaping_paths(repo: Path, stubs: dict[str, Any],
                                              monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("PGARBOR_ORACLE_REVIEW", "1")
    with pytest.raises(ValueError):
        guard.record_review(repo, "reviewer", ["../outside"], "bad path")
    assert not (repo / guard.REVIEWS_REL).exists()


@pytest.mark.parametrize("rel,expected", [
    ("tests/expected/smoke/a.json", "protected"),
    ("contracts/ARCHITECTURE.md", "protected"),
    ("harness/oracle/x.py", "protected"),
    ("harness/pgarbor_harness/config.py", "verifier"),
    ("harness/pgarbor_factory/guard.py", "verifier"),
    ("tests/cases/smoke/a.yaml", "verifier"),
    ("crates/pgarbor-core/src/lib.rs", "engine"),
    ("Cargo.toml", "engine"),
    ("Cargo.lock", "engine"),
    ("docs/notes.md", "other"),
    ("../escape/x.rs", "other"),
    ("/abs/x.rs", "other"),
])
def test_classify_path(rel: str, expected: str) -> None:
    assert guard.classify_path(rel) == expected
