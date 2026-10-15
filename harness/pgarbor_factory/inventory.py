"""Stub-marker inventory: the factory's UNIMPLEMENTED count (ARCHITECTURE §0.1).

Lease FAC-03.

Markers (exact syntactic forms the skeletons use):
  * Rust: ``PgError::unimplemented("<LEASE-ID>", "<what>")`` — regex
    ``PgError::unimplemented\\(\\s*"([A-Z][A-Z0-9]{1,9}-\\d{2,3})"\\s*,\\s*"((?:[^"\\\\]|\\\\.)*)"``
    (may span lines: run over the whole file text, compute line numbers from match.start()).
  * Python: ``NotImplementedError("<LEASE-ID>: <what>")`` — regex
    ``NotImplementedError\\(\\s*"([A-Z][A-Z0-9]{1,9}-\\d{2,3}):\\s*([^"]*)"``.
Scanned trees: ``crates/**/*.rs`` and ``harness/**/*.py`` (skip ``target/``, ``.venv/``,
``__pycache__``, ``.claude/``). Comments are NOT excluded (a commented-out marker still counts;
skeleton files never contain those).

Also counts forbidden placeholders that ARCHITECTURE §0.1 bans (reported separately, never
part of UNIMPLEMENTED): Rust ``todo!(``, ``unimplemented!(``, ``panic!(`` outside
``#[cfg(test)]`` modules (approximation: lines after a ``#[cfg(test)]`` line in the same file
are skipped), Python NotImplementedError raised WITHOUT a ``"<LEASE-ID>: "`` message prefix, ``pass  # TODO``.

``inventory(repo_root)`` -> document (canonical JSON at runs/<campaign>/inventory.json)::

    {"format": "pgarbor-inventory/1", "git_sha": ..., "total": n,
     "by_lease": {"STO-03": 4}, "by_crate": {"pgarbor-storage": 12, "harness": 80},
     "by_file": {"crates/x/src/a.rs": 3},
     "markers": [{"file", "line", "lease_id", "what"}],   # sorted by file, line
     "forbidden": [{"file", "line", "kind", "text"}]}

``crate`` for a file: ``crates/<name>/...`` -> ``<name>``; ``harness/...`` -> ``harness``.
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Any


@dataclass(frozen=True)
class Marker:
    file: str
    line: int
    lease_id: str
    what: str


def scan_file(path: Path, rel: str) -> tuple[list[Marker], list[dict[str, Any]]]:
    """(markers, forbidden findings) for one file."""
    raise NotImplementedError("FAC-03: scan one file for markers")


def scan_markers(repo_root: Path) -> list[Marker]:
    """All markers in the scanned trees, sorted by (file, line)."""
    raise NotImplementedError("FAC-03: scan repository markers")


def inventory(repo_root: Path) -> dict[str, Any]:
    """Full inventory document (module docstring)."""
    raise NotImplementedError("FAC-03: inventory document")


def delta(before: dict[str, Any], after: dict[str, Any]) -> dict[str, Any]:
    """{"total_before", "total_after", "closed_by_lease": {id: n}, "new_markers": [...]} —
    new markers appearing in a lease's files after a patch are reported (a worker may not add
    stubs for behavior it was asked to implement unless the file's lease spec allows it)."""
    raise NotImplementedError("FAC-03: inventory delta")
