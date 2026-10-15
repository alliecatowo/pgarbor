"""ORACLE-11: trusted oracle manifest (hash of corpus + verifier, separate from engine code).

Lease HAR-05.

``runs/state/oracle_manifest.json`` (written via model.write_canonical_json)::

    {
      "format": "pgarbor-oracle-manifest/1",
      "git_sha": "<HEAD sha at freeze time>",
      "reference": {"image_digest": ..., "server_version": ..., "fingerprint_sha256": ...},
      "trees": {
        "protected": {"sha256": ..., "files": {relpath: sha256}},  # config.PROTECTED_GLOBS
        "verifier":  {"sha256": ..., "files": {...}},               # config.VERIFIER_GLOBS
        "cases":     {"sha256": ..., "count": n},                    # tests/cases/**/*.yaml
        "expected":  {"sha256": ..., "count": n}                     # tests/expected/**/*.json
      },
      "pg_suites": {"regress_manifest_sha256": ..., "isolation_manifest_sha256": ...}
    }

Trees are hashed with hashing.tree_sha256(paths.root, globs) (exact algorithm documented
there). ``freeze`` writes the manifest; only the oracle review path calls it
(guarded by PGARBOR_ORACLE_REVIEW=1 like the recorder). ``check`` recomputes and returns the
list of changed/added/removed files per tree; pgarbor_factory.guard refuses a campaign when
``check`` is non-empty and the change did not come through the review path.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Optional

from ..config import Paths

MANIFEST_FORMAT = "pgarbor-oracle-manifest/1"
MANIFEST_RELPATH = "runs/state/oracle_manifest.json"

#: Globs for the corpus trees (relative to repo root).
CASES_GLOBS: tuple[str, ...] = ("tests/cases/**/*.yaml",)
EXPECTED_GLOBS: tuple[str, ...] = ("tests/expected/**",)


@dataclass
class TreeChange:
    tree: str          #: "protected" | "verifier" | "cases" | "expected"
    path: str          #: repo-relative
    change: str        #: "added" | "removed" | "modified"


@dataclass
class ManifestCheck:
    ok: bool
    changes: list[TreeChange] = field(default_factory=list)
    reference_changed: bool = False
    message: str = ""


def compute(paths: Paths, reference: Optional[dict[str, str]] = None) -> dict[str, Any]:
    """Compute the manifest document for the working tree (does not write).

    ``git_sha`` from ``git rev-parse HEAD`` (pgarbor_factory.gitutil is NOT importable from the
    harness package — use subprocess directly). ``pg_suites`` hashes are the sha256 of the
    canonical JSON of regress.manifest.build_manifest / isolation.spec_parser.build_manifest
    (call them lazily; if the PG source tree is missing record ``null``)."""
    raise NotImplementedError("HAR-05: compute manifest")


def freeze(paths: Paths, reference: dict[str, str]) -> dict[str, Any]:
    """Compute and write MANIFEST_RELPATH. Requires env PGARBOR_ORACLE_REVIEW=1, else raise
    PermissionError("oracle manifest can only be frozen through the oracle review path")."""
    raise NotImplementedError("HAR-05: freeze manifest")


def check(paths: Paths) -> ManifestCheck:
    """Load the frozen manifest and diff it against compute(). Missing manifest ->
    ManifestCheck(ok=False, message="no frozen oracle manifest")."""
    raise NotImplementedError("HAR-05: check manifest")
