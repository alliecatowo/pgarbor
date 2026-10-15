"""FAC-04 tests: per-lease worktree allocator (pgarbor_factory.worktrees).

Each test builds a throwaway git repository. It lives under a gitignored-by-convention scratch
directory inside harness/tests (never /tmp), so no worktree is ever created in a temp dir.
"""

from __future__ import annotations

import shutil
import subprocess
import tempfile
from pathlib import Path

import pytest

from pgarbor_factory import worktrees as wt
from pgarbor_factory.gitutil import GitError, head_sha

_SCRATCH_ROOT = Path(__file__).resolve().parent / ".fac04-scratch"


def _run(args: list[str], cwd: Path) -> str:
    p = subprocess.run(
        ["git", "-c", "user.name=fac04", "-c", "user.email=fac04@example.invalid",
         "-c", "commit.gpgsign=false", *args],
        cwd=str(cwd), capture_output=True, text=True, check=False,
    )
    if p.returncode != 0:
        raise AssertionError(f"git {' '.join(args)} failed: {p.stderr}")
    return p.stdout


def _commit(repo: Path, text: str) -> str:
    (repo / "f.txt").write_text(text)
    _run(["add", "f.txt"], repo)
    _run(["commit", "-q", "-m", text.strip() or "x"], repo)
    return head_sha(repo)


@pytest.fixture
def repo():
    _SCRATCH_ROOT.mkdir(exist_ok=True)
    scratch = Path(tempfile.mkdtemp(prefix="repo-", dir=_SCRATCH_ROOT))
    try:
        r = scratch / "repo"
        r.mkdir()
        _run(["init", "-q", "-b", "main"], r)
        _commit(r, "one\n")
        yield r
    finally:
        shutil.rmtree(scratch, ignore_errors=True)


def _linked_paths(repo: Path) -> list[Path]:
    main = repo.resolve()
    return [w.path for w in wt.list_worktrees(repo) if w.path.resolve() != main]


def test_allocate_creates_under_claude_worktrees(repo):
    base = head_sha(repo)
    w = wt.allocate(repo, "FAC-04", base)
    expected = repo / ".claude" / "worktrees" / "lease-fac-04"
    assert w.path.resolve() == expected.resolve()
    assert w.path.is_dir()
    assert w.branch == "lease/FAC-04"
    assert w.name == "lease-fac-04"
    assert w.head_sha == base
    assert head_sha(w.path) == base


def test_second_allocate_reuses_worktree(repo):
    base = head_sha(repo)
    w1 = wt.allocate(repo, "FAC-04", base)
    w2 = wt.allocate(repo, "FAC-04", base)
    assert w1.path.resolve() == w2.path.resolve()
    assert len(_linked_paths(repo)) == 1
    assert len(wt.list_worktrees(repo)) == 2  # main + one lease worktree


def test_reuse_keeps_worker_commits(repo):
    base = head_sha(repo)
    w = wt.allocate(repo, "FAC-04", base)
    new = _commit(w.path, "work\n")
    again = wt.allocate(repo, "FAC-04", base)
    assert again.head_sha == new


def test_remove_keeps_branch(repo):
    base = head_sha(repo)
    w = wt.allocate(repo, "FAC-04", base)
    submitted = _commit(w.path, "submission\n")
    wt.remove(repo, "lease-fac-04")
    assert not w.path.exists()
    assert _linked_paths(repo) == []
    # Branch survives removal: it is the submission.
    assert _run(["rev-parse", "--verify", "refs/heads/lease/FAC-04"], repo).strip() == submitted


def test_reallocate_after_remove_reuses_kept_branch(repo):
    base = head_sha(repo)
    w = wt.allocate(repo, "FAC-04", base)
    submitted = _commit(w.path, "submission\n")
    wt.remove(repo, "lease-fac-04")
    again = wt.allocate(repo, "FAC-04", base)
    assert again.head_sha == submitted


def test_remove_delete_branch(repo):
    base = head_sha(repo)
    wt.allocate(repo, "FAC-04", base)
    wt.remove(repo, "lease-fac-04", delete_branch=True)
    assert wt.find(repo, "lease/FAC-04") is None
    with pytest.raises(AssertionError):
        _run(["rev-parse", "--verify", "refs/heads/lease/FAC-04"], repo)


def test_remove_missing_is_idempotent(repo):
    wt.remove(repo, "lease-nope")
    assert wt.find(repo, "lease/NOPE") is None


def test_remove_delete_branch_without_worktree_raises(repo):
    with pytest.raises(GitError):
        wt.remove(repo, "lease-nope", delete_branch=True)


def test_existing_branch_without_worktree_is_not_reset(repo):
    old = head_sha(repo)
    _run(["branch", "lease/FAC-09", old], repo)
    new = _commit(repo, "advanced\n")
    w = wt.allocate(repo, "FAC-09", new)
    assert w.branch == "lease/FAC-09"
    assert w.head_sha == old
    assert new != old


def test_list_parses_main_and_linked(repo):
    base = head_sha(repo)
    wt.allocate(repo, "FAC-04", base)
    listed = {w.branch: w for w in wt.list_worktrees(repo)}
    assert "main" in listed and listed["main"].head_sha == base
    assert "lease/FAC-04" in listed
    assert listed["lease/FAC-04"].path.resolve() == (
        repo / ".claude" / "worktrees" / "lease-fac-04").resolve()


def test_find_by_branch(repo):
    base = head_sha(repo)
    assert wt.find(repo, "lease/FAC-04") is None
    w = wt.allocate(repo, "FAC-04", base)
    found = wt.find(repo, "lease/FAC-04")
    assert found is not None and found.path.resolve() == w.path.resolve()
    assert wt.find(repo, "refs/heads/lease/FAC-04") is not None


def test_assembly_worktree_reset_to_base(repo):
    base_a = head_sha(repo)
    w = wt.allocate_assembly(repo, 1, base_a)
    assert w.name == "epoch-01-assembly"
    assert w.branch == "epoch/01-assembly"
    assert w.path.resolve() == (repo / ".claude" / "worktrees" / "epoch-01-assembly").resolve()
    _commit(w.path, "assembled\n")
    base_b = _commit(repo, "newer contract\n")
    again = wt.allocate_assembly(repo, 1, base_b)
    assert again.path.resolve() == w.path.resolve()
    assert again.head_sha == base_b


def test_assembly_refuses_to_reset_over_dirty_tree(repo):
    base = head_sha(repo)
    w = wt.allocate_assembly(repo, 2, base)
    (w.path / "f.txt").write_text("local edit\n")
    newer = _commit(repo, "newer\n")
    with pytest.raises(GitError):
        wt.allocate_assembly(repo, 2, newer)


def test_epoch_out_of_range_rejected(repo):
    with pytest.raises(ValueError):
        wt.allocate_assembly(repo, 100, head_sha(repo))
    with pytest.raises(ValueError):
        wt.allocate_assembly(repo, True, head_sha(repo))  # type: ignore[arg-type]


@pytest.mark.parametrize("bad", ["", "../escape", "a/b", ".hidden", "x y", "a\\b"])
def test_rejects_unsafe_lease_ids(repo, bad):
    with pytest.raises(ValueError):
        wt.allocate(repo, bad, head_sha(repo))
    assert not (repo / ".claude").exists()


def test_no_path_outside_claude_worktrees_is_created(repo):
    base = head_sha(repo)
    wt.allocate(repo, "FAC-04", base)
    wt.allocate_assembly(repo, 0, base)
    root = (repo / ".claude" / "worktrees").resolve()
    for p in _linked_paths(repo):
        assert p.resolve().parent == root
    # Nothing else was created at the repo top level besides .claude.
    created = sorted(x.name for x in repo.iterdir() if x.name != ".git")
    assert created == [".claude", "f.txt"]


def test_allocate_refuses_to_overwrite_unregistered_dir(repo):
    stray = repo / ".claude" / "worktrees" / "lease-fac-05"
    stray.mkdir(parents=True)
    (stray / "precious.txt").write_text("keep\n")
    with pytest.raises(GitError):
        wt.allocate(repo, "FAC-05", head_sha(repo))
    assert (stray / "precious.txt").read_text() == "keep\n"
