"""FAC-10 tests: evidence pack builder, immutable save, ledger and last-evidence pointer.

Lease FAC-10 (SWARM-13). Run with ``uv run pytest tests/test_fac_10.py``.
Toolchain and manifest lookups are monkeypatched so the tests are hermetic.
"""

from __future__ import annotations

import json
import subprocess
from pathlib import Path
from types import SimpleNamespace

import pytest

from pgarbor_factory import evidence
from pgarbor_factory.records import COUNT_KEYS, EvidencePack, FamilyEvidence
from pgarbor_harness.config import PROTECTED_GLOBS, VERIFIER_GLOBS
from pgarbor_harness.hashing import tree_sha256
from pgarbor_harness.model import Outcome

REF = {
    "image_digest": "sha256:" + "0" * 64,
    "server_version": "18.6 (test)",
    "fingerprint_sha256": "ab" * 32,
}


def _git(cwd: Path, *args: str) -> str:
    p = subprocess.run(["git", *args], cwd=str(cwd), capture_output=True, text=True,
                       check=True)
    return p.stdout


@pytest.fixture
def repo(tmp_path, monkeypatch) -> Path:
    root = tmp_path / "repo"
    (root / "contracts").mkdir(parents=True)
    (root / "contracts" / "ARCHITECTURE.md").write_text("arch\n", encoding="utf-8")
    (root / "tests" / "cases" / "smoke").mkdir(parents=True)
    (root / "tests" / "cases" / "smoke" / "a.yaml").write_text("id: a\n", encoding="utf-8")
    (root / "tests" / "expected" / "smoke").mkdir(parents=True)
    (root / "tests" / "expected" / "smoke" / "a.json").write_text("{}\n", encoding="utf-8")
    _git(root, "init", "-q")
    _git(root, "add", "-A")
    _git(root, "-c", "user.name=t", "-c", "user.email=t@example.invalid",
         "commit", "-q", "-m", "init")
    monkeypatch.setattr(evidence, "_toolchain_versions",
                        lambda: {"rustc": "rustc 1.0.0 (fake)\n", "cargo": "cargo 1.0.0\n"})
    monkeypatch.setattr(evidence, "_manifest_hashes", lambda paths: (None, None))
    return root


def _fam(name: str, outcomes: dict[str, Outcome], cataloged: list[str],
         status: str = "RAN") -> FamilyEvidence:
    results = [SimpleNamespace(case_id=k, outcome=v) for k, v in outcomes.items()]
    return evidence.family_from_results(name, results, cataloged, duration_s=1.5,
                                        status=status)


def _families() -> list[FamilyEvidence]:
    storage = _fam("storage-fault", {"a": Outcome.PASS, "b": Outcome.MISMATCH,
                                     "c": Outcome.BLOCKED}, list("abcde"))
    diff = _fam("differential", {"x": Outcome.PASS, "y": Outcome.INVALID_CASE,
                                 "z": Outcome.CORRUPTION}, ["x", "y", "z", "w"])
    skipped = _fam("regress-official", {}, ["r1", "r2", "r3"],
                   status="SKIPPED_PRECONDITION")
    return [storage, diff, skipped]


def _pack(repo: Path, families: list[FamilyEvidence] | None = None) -> EvidencePack:
    return evidence.build_pack(
        repo_root=repo, campaign_id="20260101T000000Z-deadbeef", epoch=2, mode="cathedral",
        reference=REF, seeds=[1, 2, 42], started_unix=1000, finished_unix=1100,
        build={"ok": True, "duration_s": 3.0, "error_clusters": [], "log": "build.log"},
        families=families if families is not None else _families(),
        halt={"action": "CONTINUE"}, resource={"cpu_s": 1.0, "wall_s": 2.0, "max_rss_mb": 9},
    )


# ---------------------------------------------------------------------------------------------
# Family counting and denominators
# ---------------------------------------------------------------------------------------------


def test_family_counts_unrun_and_denominators():
    fam = _fam("storage-fault", {"a": Outcome.PASS, "b": Outcome.MISMATCH,
                                 "c": Outcome.BLOCKED}, list("abcde"))
    assert fam.status == "RAN"
    assert fam.counts["PASS"] == 1
    assert fam.counts["MISMATCH"] == 1
    assert fam.counts["BLOCKED"] == 1
    assert fam.counts["UNRUN"] == 2
    assert fam.total_cataloged == 5
    assert fam.runnable == 5 - (1 + 0 + 2)
    assert fam.passed == 1
    assert fam.duration_s == 1.5


def test_family_skipped_precondition_is_all_unrun():
    fam = _fam("isolation", {}, ["i1", "i2", "i3"], status="SKIPPED_PRECONDITION")
    assert fam.counts["UNRUN"] == 3
    assert fam.total_cataloged == 3
    assert fam.runnable == 0
    assert fam.passed == 0


def test_family_result_without_case_id_is_counted_once():
    results = [SimpleNamespace(outcome=Outcome.PASS), SimpleNamespace(outcome=Outcome.HANG)]
    fam = evidence.family_from_results("anon", results, [])
    assert fam.counts["PASS"] == 1 and fam.counts["HANG"] == 1
    assert fam.total_cataloged == 2 and fam.runnable == 2


def test_family_duplicate_case_id_uses_last_result():
    results = [SimpleNamespace(case_id="a", outcome=Outcome.MISMATCH),
               SimpleNamespace(case_id="a", outcome=Outcome.PASS)]
    fam = evidence.family_from_results("dup", results, ["a"])
    assert fam.counts["PASS"] == 1
    assert fam.counts["MISMATCH"] == 0
    assert fam.total_cataloged == 1


def test_family_rejects_bad_status_and_unknown_outcome():
    with pytest.raises(ValueError):
        evidence.family_from_results("f", [], [], status="WHATEVER")
    with pytest.raises(ValueError):
        evidence.family_from_results("f", [SimpleNamespace(case_id="a",
                                                           outcome="NOT_A_STATUS")], [])
    with pytest.raises(TypeError):
        evidence.family_from_results("f", [object()], [])


# ---------------------------------------------------------------------------------------------
# Pack assembly
# ---------------------------------------------------------------------------------------------


def test_build_pack_acceptance_invariants(repo):
    pack = _pack(repo)
    assert pack.format == "pgarbor-evidence/1"
    assert pack.git["sha"] == _git(repo, "rev-parse", "HEAD").strip()
    assert pack.git["dirty"] is False
    assert pack.toolchain == {"rustc": "rustc 1.0.0 (fake)\n", "cargo": "cargo 1.0.0\n"}
    assert set(pack.platform) == {"os", "kernel", "arch", "python"}
    assert pack.reference == REF
    assert set(pack.corpus) == {"cases", "expected", "verifier", "protected",
                                "regress_manifest", "isolation_manifest"}
    assert pack.corpus["regress_manifest"] is None
    for fam in pack.families:
        assert fam.runnable == (fam.total_cataloged - fam.counts["BLOCKED"]
                                - fam.counts["INVALID_CASE"] - fam.counts["UNRUN"]), fam.name
        assert fam.passed == fam.counts["PASS"]
    assert set(pack.totals) == set(COUNT_KEYS)
    for k in COUNT_KEYS:
        assert pack.totals[k] == sum(f.counts[k] for f in pack.families)
    assert pack.critical == {"corruption": 1, "crash": 0}
    assert pack.totals["CORRUPTION"] == 1


def test_build_pack_recomputes_denominators_from_counts(repo):
    bogus = FamilyEvidence(name="f", status="RAN", total_cataloged=99, runnable=99, passed=99,
                           counts={"PASS": 2, "UNRUN": 1})
    pack = _pack(repo, [bogus])
    fam = pack.families[0]
    assert fam.total_cataloged == 3
    assert fam.runnable == 2
    assert fam.passed == 2


def test_build_pack_validates_inputs(repo):
    with pytest.raises(ValueError):
        evidence.build_pack(repo, "c", 0, "nope", REF, [], 1, 2, {}, [])
    with pytest.raises(ValueError):
        evidence.build_pack(repo, "c", 0, "seams", {"image_digest": "x"}, [], 1, 2, {}, [])
    with pytest.raises(ValueError):
        evidence.build_pack(repo, "c", 0, "seams", REF, [], 5, 2, {}, [])
    dup = [_fam("a", {}, []), _fam("a", {}, [])]
    with pytest.raises(ValueError):
        evidence.build_pack(repo, "c", 0, "seams", REF, [], 1, 2, {}, dup)


def test_corpus_hash_matches_tree_sha256(repo):
    pack = _pack(repo)
    assert pack.corpus["cases"] == tree_sha256(repo, ["tests/cases/**/*.yaml"])[0]
    assert pack.corpus["expected"] == tree_sha256(repo, ["tests/expected/**"])[0]
    assert pack.corpus["verifier"] == tree_sha256(repo, VERIFIER_GLOBS)[0]
    assert pack.corpus["protected"] == tree_sha256(repo, PROTECTED_GLOBS)[0]


def test_manifest_hashes_present_when_available(repo, monkeypatch):
    monkeypatch.setattr(evidence, "_manifest_hashes", lambda paths: ("r" * 64, "i" * 64))
    pack = _pack(repo)
    assert pack.corpus["regress_manifest"] == "r" * 64
    assert pack.corpus["isolation_manifest"] == "i" * 64


# ---------------------------------------------------------------------------------------------
# Save, immutability, ledger, pointer
# ---------------------------------------------------------------------------------------------


def test_pack_json_round_trips(repo):
    pack = _pack(repo)
    run_dir = repo / "runs" / pack.campaign_id
    path = evidence.save_pack(pack, run_dir, repo)
    loaded = EvidencePack.from_json(json.loads(path.read_text(encoding="utf-8")))
    assert loaded == pack


def test_save_pack_refuses_overwrite_and_is_read_only(repo):
    pack = _pack(repo)
    run_dir = repo / "runs" / pack.campaign_id
    path = evidence.save_pack(pack, run_dir, repo)
    assert path.name == "evidence.json"
    assert path.stat().st_mode & 0o777 == 0o444
    before = path.read_bytes()

    with pytest.raises(FileExistsError):
        evidence.save_pack(pack, run_dir, repo)
    assert path.read_bytes() == before

    ledger = (repo / "runs" / "state" / "evidence_ledger.jsonl").read_text(encoding="utf-8")
    assert len(ledger.splitlines()) == 1


def test_save_pack_appends_ledger_line_and_pointer(repo):
    pack = _pack(repo)
    run_dir = repo / "runs" / pack.campaign_id
    path = evidence.save_pack(pack, run_dir, repo)

    lines = (repo / "runs" / "state" / "evidence_ledger.jsonl").read_text(
        encoding="utf-8").splitlines()
    assert len(lines) == 1
    entry = json.loads(lines[0])
    assert set(entry) == {"campaign_id", "git_sha", "path", "sha256", "totals"}
    assert entry["campaign_id"] == pack.campaign_id
    assert entry["git_sha"] == pack.git["sha"]
    assert entry["path"] == path.relative_to(repo).as_posix()
    assert entry["totals"] == pack.totals
    assert len(entry["sha256"]) == 64

    pointer = json.loads((repo / "runs" / "state" / "last_evidence.json").read_text(
        encoding="utf-8"))
    assert pointer["campaign_id"] == pack.campaign_id
    assert pointer["sha256"] == entry["sha256"]


def test_ledger_grows_one_line_per_campaign(repo):
    p1 = _pack(repo)
    evidence.save_pack(p1, repo / "runs" / "c1", repo)
    p2 = evidence.build_pack(repo, "c2", 3, "seams", REF, [1], 1, 2, {}, _families())
    evidence.save_pack(p2, repo / "runs" / "c2", repo)
    lines = (repo / "runs" / "state" / "evidence_ledger.jsonl").read_text(
        encoding="utf-8").splitlines()
    assert [json.loads(x)["campaign_id"] for x in lines] == [p1.campaign_id, "c2"]


def test_load_last_returns_previous_pack(repo):
    assert evidence.load_last(repo) is None
    pack = _pack(repo)
    evidence.save_pack(pack, repo / "runs" / pack.campaign_id, repo)
    last = evidence.load_last(repo)
    assert last == pack


def test_load_last_detects_altered_pack(repo):
    pack = _pack(repo)
    path = evidence.save_pack(pack, repo / "runs" / pack.campaign_id, repo)
    path.chmod(0o644)
    path.write_text("{}\n", encoding="utf-8")
    with pytest.raises(ValueError):
        evidence.load_last(repo)
