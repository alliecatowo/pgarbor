"""FAC-06 tests: deterministic epoch assembly in a throwaway repo.

Sibling factory modules (worktrees, leases, submissions, review) are replaced with small fakes
so these tests exercise only the assembler's own logic.
"""

from __future__ import annotations

import json
import subprocess
from pathlib import Path
from types import SimpleNamespace

import pytest

from pgarbor_factory import assemble, gitutil
from pgarbor_factory.records import LeaseSpec, LeaseState, Submission


def _git(repo: Path, *args: str) -> str:
    p = subprocess.run(["git", "-c", "commit.gpgsign=false", *args], cwd=str(repo),
                       capture_output=True, text=True, check=True)
    return p.stdout.strip()


def _write(repo: Path, rel: str, text: str) -> None:
    p = repo / rel
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(text, encoding="utf-8")


def _commit_all(repo: Path, msg: str) -> str:
    _git(repo, "add", "-A")
    _git(repo, "commit", "-q", "-m", msg)
    return _git(repo, "rev-parse", "HEAD")


@pytest.fixture()
def repo(tmp_path: Path) -> Path:
    r = tmp_path / "repo"
    r.mkdir()
    _git(r, "init", "-q", "-b", "main")
    _git(r, "config", "user.name", "T")
    _git(r, "config", "user.email", "t@example.invalid")
    _write(r, "a/x.txt", "x1\nx2\nx3\n")
    _write(r, "b/y.txt", "y1\ny2\ny3\n")
    _write(r, "c/z.txt", "z1\n")
    _write(r, "contracts/x", "frozen\n")
    _write(r, "tests/expected/e.out", "e\n")
    _write(r, "harness/h.py", "h\n")
    _commit_all(r, "base")
    return r


def _branch(repo: Path, name: str, base: str, edits: dict[str, str | None]) -> str:
    _git(repo, "checkout", "-q", "-b", name, base)
    for rel, text in edits.items():
        if text is None:
            _git(repo, "rm", "-q", rel)
        else:
            _write(repo, rel, text)
    sha = _commit_all(repo, name)
    _git(repo, "checkout", "-q", "main")
    return sha


def _spec(lid: str, crate: str, files: list[str]) -> LeaseSpec:
    return LeaseSpec(lease_id=lid, crate=crate, task_ids=[], files=files, model_tier="sonnet",
                     summary="", spec="")


def _sub(lid: str, base: str, head: str) -> Submission:
    return Submission(lease_id=lid, worker="w", base_sha=base, branch=f"lease/{lid}",
                      head_sha=head)


@pytest.fixture()
def env(repo: Path, monkeypatch: pytest.MonkeyPatch):
    state = SimpleNamespace(specs=[], subs={}, transitions=[], findings=[])

    def allocate_assembly(repo_root: Path, epoch: int, base_sha: str):
        path = repo_root / ".claude" / "worktrees" / f"epoch-{epoch:02d}-assembly"
        branch = f"epoch/{epoch:02d}-assembly"
        if path.exists():
            _git(path, "checkout", "-q", "-B", branch, base_sha)
        else:
            _git(repo_root, "worktree", "add", "-q", "-B", branch, str(path), base_sha)
        return SimpleNamespace(name=path.name, path=path, branch=branch, head_sha=base_sha)

    monkeypatch.setattr(assemble.worktrees, "allocate_assembly", allocate_assembly)
    monkeypatch.setattr(assemble.leases, "load_catalog", lambda d: list(state.specs))
    monkeypatch.setattr(
        assemble.leases, "load_ledger",
        lambda p: {lid: SimpleNamespace(state=LeaseState.PATCH_SUBMITTED) for lid in state.subs})
    monkeypatch.setattr(assemble.leases, "transition",
                        lambda p, lid, to, note="", **kw: state.transitions.append((lid, to, note)))
    monkeypatch.setattr(assemble.submissions, "load_submissions", lambda r: dict(state.subs))
    monkeypatch.setattr(assemble.review, "review_patch", lambda *a, **k: list(state.findings))
    return state


def _rejected(rep, lid: str):
    return next(r for r in rep.rejected if r.lease_id == lid)


def test_disjoint_leases_assemble_deterministically(repo: Path, env) -> None:
    base = _git(repo, "rev-parse", "HEAD")
    ha = _branch(repo, "lease/A-01", base, {"a/x.txt": "x1\nX2\nx3\n"})
    hb = _branch(repo, "lease/B-01", base, {"b/y.txt": "y1\nY2\ny3\n"})
    env.specs = [_spec("A-01", "pgarbor-common", ["a/x.txt"]),
                 _spec("B-01", "pgarbor-storage", ["b/y.txt"])]
    env.subs = {"B-01": _sub("B-01", base, hb), "A-01": _sub("A-01", base, ha)}

    r1 = assemble.assemble_epoch(repo, 1, base)
    r2 = assemble.assemble_epoch(repo, 1, base)
    assert r1.accepted == ["A-01", "B-01"]
    assert not r1.rejected
    assert r1.epoch_sha == r2.epoch_sha != base
    wt = repo / ".claude" / "worktrees" / "epoch-01-assembly"
    assert (wt / "a/x.txt").read_text() == "x1\nX2\nx3\n"
    assert (wt / "b/y.txt").read_text() == "y1\nY2\ny3\n"
    assert _git(wt, "log", "-1", "--format=%an <%ae>|%s") == (
        f"PGArbor Assembler <assembler@pgarbor.invalid>|assemble B-01 {hb[:12]}")
    assert _git(wt, "log", "-1", "--format=%at") == str(86400 + 1)
    doc = json.loads((repo / "runs" / "epoch-01" / "assembly.json").read_text())
    assert doc["accepted"] == ["A-01", "B-01"] and doc["epoch_sha"] == r1.epoch_sha
    assert [t[:2] for t in env.transitions[:2]] == [("A-01", LeaseState.ASSEMBLED),
                                                   ("B-01", LeaseState.ASSEMBLED)]


def test_ordering_by_crate_then_id(repo: Path) -> None:
    specs = {"Z-01": _spec("Z-01", "pgarbor-common", []), "A-02": _spec("A-02", "harness", []),
             "A-01": _spec("A-01", "pgarbor-storage", []), "A-03": _spec("A-03", "harness", [])}
    subs = [_sub(k, "b", "h") for k in ("A-03", "A-01", "A-02", "Z-01")]
    out = assemble.order_candidates(specs, subs)
    assert [s.lease_id for s in out] == ["Z-01", "A-01", "A-02", "A-03"]


def test_protected_path_rejected(repo: Path, env) -> None:
    base = _git(repo, "rev-parse", "HEAD")
    h1 = _branch(repo, "lease/A-01", base, {"contracts/x": "evil\n"})
    h2 = _branch(repo, "lease/B-01", base, {"tests/expected/e.out": "evil\n"})
    env.specs = [_spec("A-01", "pgarbor-common", ["contracts/x"]),
                 _spec("B-01", "pgarbor-storage", ["tests/expected/e.out"])]
    env.subs = {"A-01": _sub("A-01", base, h1), "B-01": _sub("B-01", base, h2)}
    rep = assemble.assemble_epoch(repo, 2, base)
    assert rep.accepted == []
    assert _rejected(rep, "A-01").reason == "protected_path"
    assert _rejected(rep, "B-01").reason == "protected_path"
    assert rep.epoch_sha == base


def test_unleased_path_rejected_and_worktree_clean(repo: Path, env) -> None:
    base = _git(repo, "rev-parse", "HEAD")
    h = _branch(repo, "lease/A-01", base, {"a/x.txt": "x1\nX2\nx3\n", "c/z.txt": "zz\n"})
    env.specs = [_spec("A-01", "pgarbor-common", ["a/x.txt"])]
    env.subs = {"A-01": _sub("A-01", base, h)}
    rep = assemble.assemble_epoch(repo, 3, base)
    r = _rejected(rep, "A-01")
    assert r.reason == "unleased_path" and r.paths == ["c/z.txt"]
    wt = repo / ".claude" / "worktrees" / "epoch-03-assembly"
    assert _git(wt, "status", "--porcelain") == ""
    assert (wt / "a/x.txt").read_text() == "x1\nx2\nx3\n"
    assert env.transitions == [("A-01", LeaseState.REJECTED, "unleased_path")]


def test_rust_lease_touching_harness_is_verifier_path(repo: Path, env) -> None:
    base = _git(repo, "rev-parse", "HEAD")
    _write(repo, "harness/pgarbor_factory/q.py", "q\n")
    base = _commit_all(repo, "add q")
    h = _branch(repo, "lease/A-01", base, {"harness/pgarbor_factory/q.py": "q2\n"})
    env.specs = [_spec("A-01", "pgarbor-common", ["harness/pgarbor_factory/q.py"])]
    env.subs = {"A-01": _sub("A-01", base, h)}
    rep = assemble.assemble_epoch(repo, 4, base)
    assert _rejected(rep, "A-01").reason == "verifier_path"
    # the same file is fine for a harness lease
    env.specs = [_spec("FAC-99", "harness", ["harness/pgarbor_factory/q.py"])]
    env.subs = {"FAC-99": _sub("FAC-99", base, h)}
    rep = assemble.assemble_epoch(repo, 4, base)
    assert rep.accepted == ["FAC-99"]


def test_structural_and_deletion_rejected(repo: Path, env) -> None:
    base = _git(repo, "rev-parse", "HEAD")
    _write(repo, "a/lib.rs", "//\n")
    base = _commit_all(repo, "add lib")
    h1 = _branch(repo, "lease/A-01", base, {"a/lib.rs": "// changed\n"})
    h2 = _branch(repo, "lease/B-01", base, {"b/y.txt": None})
    env.specs = [_spec("A-01", "pgarbor-common", ["a/lib.rs"]),
                 _spec("B-01", "pgarbor-storage", ["b/y.txt"])]
    env.subs = {"A-01": _sub("A-01", base, h1), "B-01": _sub("B-01", base, h2)}
    rep = assemble.assemble_epoch(repo, 5, base)
    assert _rejected(rep, "A-01").reason == "structural_file"
    assert _rejected(rep, "B-01").reason == "deletion"


def test_stale_base_rejected(repo: Path, env) -> None:
    old = _git(repo, "rev-parse", "HEAD")
    side = _branch(repo, "side", old, {"c/z.txt": "side\n"})
    h = _branch(repo, "lease/A-01", side, {"a/x.txt": "x1\nX2\nx3\n"})
    env.specs = [_spec("A-01", "pgarbor-common", ["a/x.txt"])]
    env.subs = {"A-01": _sub("A-01", side, h)}
    rep = assemble.assemble_epoch(repo, 6, old)
    assert _rejected(rep, "A-01").reason == "stale_base"


def test_review_reject_blocks(repo: Path, env) -> None:
    base = _git(repo, "rev-parse", "HEAD")
    h = _branch(repo, "lease/A-01", base, {"a/x.txt": "x1\nX2\nx3\n"})
    env.specs = [_spec("A-01", "pgarbor-common", ["a/x.txt"])]
    env.subs = {"A-01": _sub("A-01", base, h)}
    env.findings = [{"rule": "fake_success", "severity": "reject", "file": "a/x.txt",
                     "line": 1, "detail": ""},
                    {"rule": "large_file", "severity": "warn", "file": "a/x.txt",
                     "line": 1, "detail": ""}]
    rep = assemble.assemble_epoch(repo, 7, base)
    r = _rejected(rep, "A-01")
    assert r.reason == "review:fake_success" and r.paths == ["a/x.txt"]


def test_conflict_names_first_lease(repo: Path, env) -> None:
    base = _git(repo, "rev-parse", "HEAD")
    h1 = _branch(repo, "lease/A-01", base, {"a/x.txt": "x1\nONE\nx3\n"})
    h2 = _branch(repo, "lease/B-01", base, {"a/x.txt": "x1\nTWO\nx3\n"})
    # Overlapping leases are invalid in a real catalog; here they force a conflict.
    env.specs = [_spec("A-01", "pgarbor-common", ["a/x.txt"]),
                 _spec("B-01", "pgarbor-storage", ["a/x.txt"])]
    env.subs = {"A-01": _sub("A-01", base, h1), "B-01": _sub("B-01", base, h2)}
    rep = assemble.assemble_epoch(repo, 8, base)
    assert rep.accepted == ["A-01"]
    r = _rejected(rep, "B-01")
    assert r.reason.startswith("conflict") and "A-01" in r.reason
    assert r.paths == ["a/x.txt"]
    wt = repo / ".claude" / "worktrees" / "epoch-08-assembly"
    assert _git(wt, "status", "--porcelain") == ""
    assert (wt / "a/x.txt").read_text() == "x1\nONE\nx3\n"
    assert gitutil.head_sha(wt) == rep.epoch_sha
