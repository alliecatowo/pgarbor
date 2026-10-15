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

from dataclasses import dataclass
from pathlib import Path
from typing import Optional


@dataclass
class Worktree:
    name: str
    path: Path
    branch: str
    head_sha: str


def list_worktrees(repo_root: Path) -> list[Worktree]:
    """Parse ``git worktree list --porcelain``."""
    raise NotImplementedError("FAC-04: list worktrees")


def allocate(repo_root: Path, lease_id: str, base_sha: str) -> Worktree:
    """Create or reuse the worktree for a lease (rules above)."""
    raise NotImplementedError("FAC-04: allocate lease worktree")


def allocate_assembly(repo_root: Path, epoch: int, base_sha: str) -> Worktree:
    """Worktree ``epoch-<NN>-assembly`` on branch ``epoch/<NN>-assembly`` reset to base_sha
    (``git checkout -B``); reused across retries of the same epoch."""
    raise NotImplementedError("FAC-04: allocate assembly worktree")


def remove(repo_root: Path, name: str, delete_branch: bool = False) -> None:
    raise NotImplementedError("FAC-04: remove worktree")


def find(repo_root: Path, branch: str) -> Optional[Worktree]:
    raise NotImplementedError("FAC-04: find worktree by branch")
