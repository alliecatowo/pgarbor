"""SWARM-04: isolated per-lease worktree allocator.

Lease FAC-04.

Rules (user's global rules + RFC 003 §4):
  * worktrees live ONLY under ``<repo>/.claude/worktrees/<name>`` (never /tmp, never the
    scratchpad); name = ``lease-<lease_id lowercase>`` for workers, ``epoch-<NN>-assembly``
    for the assembler;
  * branch = ``lease/<LEASE-ID>`` created from the frozen contract SHA (``base_sha``) when it
    does not exist; an existing worktree for the same branch is REUSED (``git worktree list
    --porcelain`` parse), never duplicated;
  * no ``cargo``/``npm``/``uv sync`` is ever run inside a worker worktree by the factory
    (cathedral mode). Shared build caches: the assembler sets ``CARGO_TARGET_DIR`` to the main
    repo's ``target/`` (one cache).
  * ``remove(name)``: ``git worktree remove --force`` then ``git worktree prune``; the branch is
    kept (it is the submission) unless ``delete_branch``.
"""

from __future__ import annotations

import re
from dataclasses import dataclass
from pathlib import Path
from typing import Optional

from .gitutil import GitError, git, head_sha, rev_parse


@dataclass
class Worktree:
    name: str
    path: Path
    branch: str
    head_sha: str


# A lease id / worktree name is a single safe path component: no separators, no leading dot.
_SAFE_ID = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]{0,127}")
_BRANCH_PREFIX = "refs/heads/"


def _worktrees_dir(repo_root: Path) -> Path:
    return repo_root / ".claude" / "worktrees"


def _check_id(value: str, what: str) -> None:
    if not _SAFE_ID.fullmatch(value):
        raise ValueError(f"invalid {what}: {value!r}")


def _lease_path(repo_root: Path, name: str) -> Path:
    """Path of a worktree named ``name``; always directly under <repo>/.claude/worktrees."""
    _check_id(name, "worktree name")
    base = _worktrees_dir(repo_root)
    path = base / name
    if path.parent != base:  # pragma: no cover - defensive, _SAFE_ID already excludes '/'
        raise ValueError(f"worktree path escapes worktrees dir: {name!r}")
    return path


def _same_path(a: Path, b: Path) -> bool:
    return a.resolve() == b.resolve()


def _parse_porcelain(text: str) -> list[Worktree]:
    """Parse ``git worktree list --porcelain`` blocks (separated by blank lines)."""
    out: list[Worktree] = []
    for block in text.split("\n\n"):
        path_s: Optional[str] = None
        sha = ""
        branch = ""
        for line in block.splitlines():
            if line.startswith("worktree "):
                path_s = line[len("worktree "):]
            elif line.startswith("HEAD "):
                sha = line[len("HEAD "):]
            elif line.startswith("branch "):
                ref = line[len("branch "):]
                branch = ref[len(_BRANCH_PREFIX):] if ref.startswith(_BRANCH_PREFIX) else ref
            # "detached", "bare", "locked [reason]", "prunable [reason]": no branch name.
        if path_s is None:
            continue
        p = Path(path_s)
        out.append(Worktree(name=p.name, path=p, branch=branch, head_sha=sha))
    return out


def list_worktrees(repo_root: Path) -> list[Worktree]:
    """Parse ``git worktree list --porcelain``."""
    return _parse_porcelain(git(["worktree", "list", "--porcelain"], repo_root))


def find(repo_root: Path, branch: str) -> Optional[Worktree]:
    """Worktree that has ``branch`` checked out (``lease/FAC-04``), or None."""
    if branch.startswith(_BRANCH_PREFIX):
        branch = branch[len(_BRANCH_PREFIX):]
    for wt in list_worktrees(repo_root):
        if wt.branch == branch:
            return wt
    return None


def _find_by_path(repo_root: Path, path: Path) -> Optional[Worktree]:
    for wt in list_worktrees(repo_root):
        if _same_path(wt.path, path):
            return wt
    return None


def _branch_exists(repo_root: Path, branch: str) -> bool:
    try:
        rev_parse(f"{_BRANCH_PREFIX}{branch}", repo_root)
    except GitError:
        return False
    return True


def _ensure(repo_root: Path, name: str, branch: str, base_sha: str, reset: bool) -> Worktree:
    """Create the worktree ``name`` on ``branch``, or reuse the one already holding it.

    ``reset=False`` (lease workers): an existing branch is kept as-is; a missing branch is
    created at ``base_sha``. ``reset=True`` (assembly): the branch is force-set to ``base_sha``
    with ``checkout -B`` / ``worktree add -B``; local edits are never discarded implicitly.
    """
    path = _lease_path(repo_root, name)
    # Drop registrations whose directories were deleted by hand, so reuse sees the truth.
    git(["worktree", "prune"], repo_root)

    existing = find(repo_root, branch)
    if existing is not None:
        if _same_path(existing.path, repo_root):
            raise GitError(f"branch {branch} is checked out in the main worktree; refusing to use it")
        if reset:
            git(["checkout", "-B", branch, rev_parse(base_sha, repo_root)], existing.path)
        return Worktree(name=existing.path.name, path=existing.path, branch=branch,
                        head_sha=head_sha(existing.path))

    if path.exists() or path.is_symlink():
        raise GitError(f"{path} exists but is not a registered worktree; refusing to overwrite")
    path.parent.mkdir(parents=True, exist_ok=True)

    if reset:
        git(["worktree", "add", "-B", branch, str(path), rev_parse(base_sha, repo_root)],
            repo_root)
    elif _branch_exists(repo_root, branch):
        git(["worktree", "add", str(path), branch], repo_root)
    else:
        git(["worktree", "add", "-b", branch, str(path), rev_parse(base_sha, repo_root)],
            repo_root)
    return Worktree(name=name, path=path, branch=branch, head_sha=head_sha(path))


def allocate(repo_root: Path, lease_id: str, base_sha: str) -> Worktree:
    """Create or reuse the worktree for a lease (rules above)."""
    _check_id(lease_id, "lease id")
    name = f"lease-{lease_id.lower()}"
    return _ensure(repo_root, name, f"lease/{lease_id}", base_sha, reset=False)


def allocate_assembly(repo_root: Path, epoch: int, base_sha: str) -> Worktree:
    """Worktree ``epoch-<NN>-assembly`` on branch ``epoch/<NN>-assembly`` reset to base_sha
    (``git checkout -B``); reused across retries of the same epoch."""
    if isinstance(epoch, bool) or not isinstance(epoch, int) or not 0 <= epoch <= 99:
        raise ValueError(f"epoch must be an int in 0..99, got {epoch!r}")
    nn = f"{epoch:02d}"
    return _ensure(repo_root, f"epoch-{nn}-assembly", f"epoch/{nn}-assembly", base_sha,
                   reset=True)


def remove(repo_root: Path, name: str, delete_branch: bool = False) -> None:
    """Remove the worktree ``name`` (``--force``), prune, keep the branch unless asked.

    Idempotent when nothing is registered at that path. ``delete_branch`` needs the registered
    worktree (to learn its branch) and raises GitError otherwise.
    """
    path = _lease_path(repo_root, name)
    wt = _find_by_path(repo_root, path)
    if wt is None and delete_branch:
        raise GitError(f"no registered worktree {name}; cannot determine its branch to delete")
    if wt is not None:
        if _same_path(wt.path, repo_root):
            raise GitError("refusing to remove the main worktree")
        if wt.path.is_dir():
            git(["worktree", "remove", "--force", str(wt.path)], repo_root)
    git(["worktree", "prune"], repo_root)
    if delete_branch and wt is not None and wt.branch:
        git(["branch", "-D", wt.branch], repo_root)
