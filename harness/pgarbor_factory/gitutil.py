"""Thin git wrappers (subprocess, no shell). CONTRACT; implemented, frozen."""

from __future__ import annotations

import subprocess
from pathlib import Path
from typing import Sequence


class GitError(RuntimeError):
    pass


def git(args: Sequence[str], cwd: Path, check: bool = True, input_text: str | None = None) -> str:
    """Run ``git <args>`` in cwd; return stdout (text). Non-zero exit with check -> GitError
    containing the command and stderr."""
    p = subprocess.run(["git", *args], cwd=str(cwd), capture_output=True, text=True,
                       input=input_text, check=False)
    if check and p.returncode != 0:
        raise GitError(f"git {' '.join(args)} failed ({p.returncode}): {p.stderr.strip()}")
    return p.stdout


def rev_parse(ref: str, cwd: Path) -> str:
    return git(["rev-parse", "--verify", f"{ref}^{{commit}}"], cwd).strip()


def head_sha(cwd: Path) -> str:
    return rev_parse("HEAD", cwd)


def current_branch(cwd: Path) -> str:
    return git(["rev-parse", "--abbrev-ref", "HEAD"], cwd).strip()


def is_dirty(cwd: Path) -> bool:
    return bool(git(["status", "--porcelain"], cwd).strip())


def changed_files(base: str, head: str, cwd: Path) -> list[tuple[str, str]]:
    """``git diff --name-status --no-renames -z base head`` -> [(status letter, path)].
    Renames are reported as delete + add (``--no-renames``) so both paths get checked."""
    out = git(["diff", "--name-status", "--no-renames", "-z", base, head], cwd)
    parts = [p for p in out.split("\0") if p]
    res = []
    for i in range(0, len(parts) - 1, 2):
        res.append((parts[i][0], parts[i + 1]))
    return res


def merge_base(a: str, b: str, cwd: Path) -> str:
    return git(["merge-base", a, b], cwd).strip()


def repo_root(cwd: Path) -> Path:
    return Path(git(["rev-parse", "--show-toplevel"], cwd).strip())
