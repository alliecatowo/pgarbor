"""Content hashing used by expected files, oracle manifest, guard and evidence packs.

CONTRACT (harness v1). Implemented; frozen.

Tree hash algorithm (byte-exact, so independent tools can reproduce it):
  1. collect files under ``root`` matching any glob (``glob_match`` semantics below), skipping
     ``__pycache__`` dirs, ``*.pyc`` and ``.git``;
  2. sort by repo-relative POSIX path (Python str ordering = code point order);
  3. feed ``f"{relpath}\\0{sha256_hex(file bytes)}\\n"`` for each into one sha256;
  4. result is lowercase hex. Empty set hashes the empty input
     (``e3b0c442...b855``).
"""

from __future__ import annotations

import hashlib
import re
from functools import lru_cache
from pathlib import Path
from typing import Iterable

EMPTY_SHA256 = hashlib.sha256(b"").hexdigest()


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def case_sha256(raw: bytes) -> str:
    """Hash of a case file's exact bytes (no normalization: any edit invalidates the
    recorded expectation)."""
    return sha256_bytes(raw)


@lru_cache(maxsize=512)
def _glob_regex(pattern: str) -> re.Pattern[str]:
    # '**' matches any number of path segments (including none), '*' matches within a segment,
    # '?' one non-'/' char. Everything else literal.
    out = []
    i = 0
    while i < len(pattern):
        c = pattern[i]
        if pattern.startswith("**/", i):
            out.append("(?:.*/)?")
            i += 3
        elif pattern.startswith("**", i):
            out.append(".*")
            i += 2
        elif c == "*":
            out.append("[^/]*")
            i += 1
        elif c == "?":
            out.append("[^/]")
            i += 1
        else:
            out.append(re.escape(c))
            i += 1
    return re.compile("^" + "".join(out) + "$")


def glob_match(relpath: str, pattern: str) -> bool:
    """Match a repo-relative POSIX path against a glob where ``**`` spans directories.

    >>> glob_match("contracts/oracle/case.schema.json", "contracts/**")
    True
    >>> glob_match("crates/x/src/lib.rs", "crates/*/src/lib.rs")
    True
    >>> glob_match("crates/x/src/a/lib.rs", "crates/*/src/lib.rs")
    False
    """
    return bool(_glob_regex(pattern).match(relpath))


def matches_any(relpath: str, patterns: Iterable[str]) -> bool:
    return any(glob_match(relpath, p) for p in patterns)


def _skip(rel: str) -> bool:
    parts = rel.split("/")
    return "__pycache__" in parts or ".git" in parts or rel.endswith(".pyc")


def list_tree(root: Path, patterns: Iterable[str]) -> list[str]:
    """Sorted repo-relative POSIX paths of regular files under root matching any pattern."""
    pats = tuple(patterns)
    out = []
    for p in root.rglob("*"):
        if not p.is_file():
            continue
        rel = p.relative_to(root).as_posix()
        if _skip(rel) or not matches_any(rel, pats):
            continue
        out.append(rel)
    return sorted(out)


def tree_sha256(root: Path, patterns: Iterable[str]) -> tuple[str, dict[str, str]]:
    """Return (tree hash, {relpath: file sha256}) per the module algorithm."""
    files = {rel: sha256_file(root / rel) for rel in list_tree(root, patterns)}
    h = hashlib.sha256()
    for rel in sorted(files):
        h.update(f"{rel}\0{files[rel]}\n".encode("utf-8"))
    return h.hexdigest(), files
