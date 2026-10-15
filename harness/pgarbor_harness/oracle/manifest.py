"""ORACLE-11: trusted oracle manifest (hash of corpus + verifier, separate from engine code).

Lease HAR-05.

``runs/state/oracle_manifest.json`` (written via model.write_canonical_json)::

    {
      "format": "pgarbor-oracle-manifest/1",
      "git_sha": "<HEAD sha at freeze time>",
      "reference": {"image_digest": ..., "server_version": ..., "fingerprint_sha256": ...},
      "trees": {
        "protected": {"sha256": ..., "count": n, "files": {relpath: sha256}},  # PROTECTED_GLOBS
        "verifier":  {"sha256": ..., "count": n, "files": {...}},               # VERIFIER_GLOBS
        "cases":     {"sha256": ..., "count": n, "files": {...}},               # tests/cases/**/*.yaml
        "expected":  {"sha256": ..., "count": n, "files": {...}}                # tests/expected/**
      },
      "pg_suites": {"regress_manifest_sha256": ..., "isolation_manifest_sha256": ...}
    }

Trees are hashed with hashing.tree_sha256(paths.root, globs) (exact algorithm documented
there). ``freeze`` writes the manifest; only the oracle review path calls it
(guarded by PGARBOR_ORACLE_REVIEW=1 like the recorder). ``check`` recomputes and returns the
list of changed/added/removed files per tree; pgarbor_factory.guard refuses a campaign when
``check`` is non-empty and the change did not come through the review path.

Deviation from the first draft of this docstring (documented as a contract note): ``cases`` and
``expected`` also carry a ``files`` map. A per-file map is required to report which
``tests/expected/x.json`` changed (acceptance criterion of HAR-05); the tree ``sha256`` alone
cannot say which file.

``check`` also compares the frozen reference image digest and server version against the pinned
config values, and the two pg_suites hashes against a fresh computation. The live fingerprint
sha256 needs a running container and is therefore not re-checked here (oracle.fingerprint does).
"""

from __future__ import annotations

import json
import os
import subprocess
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Optional

from ..config import PG_IMAGE_DIGEST, PG_SERVER_VERSION, PROTECTED_GLOBS, VERIFIER_GLOBS, Paths
from ..hashing import sha256_bytes, tree_sha256
from ..model import canonical_json, write_canonical_json

MANIFEST_FORMAT = "pgarbor-oracle-manifest/1"
MANIFEST_RELPATH = "runs/state/oracle_manifest.json"

#: Globs for the corpus trees (relative to repo root).
CASES_GLOBS: tuple[str, ...] = ("tests/cases/**/*.yaml",)
EXPECTED_GLOBS: tuple[str, ...] = ("tests/expected/**",)

#: Tree name -> globs, in manifest order.
TREE_GLOBS: tuple[tuple[str, tuple[str, ...]], ...] = (
    ("protected", PROTECTED_GLOBS),
    ("verifier", VERIFIER_GLOBS),
    ("cases", CASES_GLOBS),
    ("expected", EXPECTED_GLOBS),
)

_PG_SUITE_KEYS: tuple[str, ...] = ("regress_manifest_sha256", "isolation_manifest_sha256")
_REFERENCE_KEYS: tuple[str, ...] = ("image_digest", "server_version", "fingerprint_sha256")
_REVIEW_ENV = "PGARBOR_ORACLE_REVIEW"


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


def _git_sha(root: Path) -> Optional[str]:
    """HEAD sha of the repo at ``root``, or None when git is unavailable or root is no repo."""
    try:
        proc = subprocess.run(
            ["git", "rev-parse", "HEAD"],
            cwd=str(root),
            capture_output=True,
            text=True,
            timeout=60,
            check=False,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if proc.returncode != 0:
        return None
    sha = proc.stdout.strip()
    return sha or None


def _reference_section(reference: Optional[dict[str, Any]]) -> dict[str, Optional[str]]:
    """Pinned identity defaults, overlaid with any supplied (known) keys."""
    out: dict[str, Optional[str]] = {
        "image_digest": PG_IMAGE_DIGEST,
        "server_version": PG_SERVER_VERSION,
        "fingerprint_sha256": None,
    }
    if reference:
        unknown = sorted(set(reference) - set(_REFERENCE_KEYS))
        if unknown:
            raise ValueError(f"unknown reference keys: {unknown}")
        for key in _REFERENCE_KEYS:
            if key in reference:
                out[key] = reference[key]
    return out


def _trees(paths: Paths) -> dict[str, dict[str, Any]]:
    trees: dict[str, dict[str, Any]] = {}
    for name, globs in TREE_GLOBS:
        digest, files = tree_sha256(paths.root, globs)
        trees[name] = {
            "sha256": digest,
            "count": len(files),
            "files": dict(sorted(files.items())),
        }
    return trees


def compute(paths: Paths, reference: Optional[dict[str, str]] = None) -> dict[str, Any]:
    """Compute the manifest document for the working tree (does not write).

    ``git_sha`` from ``git rev-parse HEAD`` (pgarbor_factory.gitutil is NOT importable from the
    harness package — use subprocess directly). ``pg_suites`` hashes are the sha256 of the
    canonical JSON of regress.manifest.build_manifest / isolation.spec_parser.build_manifest
    (call them lazily; if the PG source tree is missing record ``null``)."""
    pg_suites: dict[str, Optional[str]] = {key: None for key in _PG_SUITE_KEYS}

    try:
        from ..regress.manifest import build_manifest as build_regress_manifest

        regress_doc = build_regress_manifest(paths.pg_regress_dir)
        pg_suites["regress_manifest_sha256"] = sha256_bytes(
            canonical_json(regress_doc).encode("utf-8")
        )
    except Exception:  # PG source missing or suite manifest unavailable: record null
        pg_suites["regress_manifest_sha256"] = None

    try:
        from ..isolation.spec_parser import build_manifest as build_isolation_manifest

        isolation_doc = build_isolation_manifest(paths.pg_isolation_dir)
        pg_suites["isolation_manifest_sha256"] = sha256_bytes(
            canonical_json(isolation_doc).encode("utf-8")
        )
    except Exception:  # PG source missing or suite manifest unavailable: record null
        pg_suites["isolation_manifest_sha256"] = None

    return {
        "format": MANIFEST_FORMAT,
        "git_sha": _git_sha(paths.root),
        "reference": _reference_section(reference),
        "trees": _trees(paths),
        "pg_suites": pg_suites,
    }


def freeze(paths: Paths, reference: dict[str, str]) -> dict[str, Any]:
    """Compute and write MANIFEST_RELPATH. Requires env PGARBOR_ORACLE_REVIEW=1, else raise
    PermissionError("oracle manifest can only be frozen through the oracle review path")."""
    if os.environ.get(_REVIEW_ENV) != "1":
        raise PermissionError("oracle manifest can only be frozen through the oracle review path")

    ref = _reference_section(reference)
    if ref["image_digest"] != PG_IMAGE_DIGEST:
        raise ValueError(
            f"reference image_digest {ref['image_digest']!r} is not the pinned {PG_IMAGE_DIGEST!r}"
        )
    if ref["server_version"] != PG_SERVER_VERSION:
        raise ValueError(
            f"reference server_version {ref['server_version']!r} is not the pinned "
            f"{PG_SERVER_VERSION!r}"
        )
    fingerprint = ref["fingerprint_sha256"]
    if not isinstance(fingerprint, str) or not fingerprint:
        raise ValueError("freeze requires the live reference fingerprint_sha256")

    doc = compute(paths, ref)
    write_canonical_json(paths.root / MANIFEST_RELPATH, doc)
    return doc


def check(paths: Paths) -> ManifestCheck:
    """Load the frozen manifest and diff it against compute(). Missing manifest ->
    ManifestCheck(ok=False, message="no frozen oracle manifest")."""
    manifest_file = paths.root / MANIFEST_RELPATH
    if not manifest_file.is_file():
        return ManifestCheck(ok=False, message="no frozen oracle manifest")

    try:
        frozen = json.loads(manifest_file.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
        return ManifestCheck(ok=False, message=f"frozen oracle manifest unreadable: {exc}")

    if not isinstance(frozen, dict) or frozen.get("format") != MANIFEST_FORMAT:
        return ManifestCheck(ok=False, message="frozen oracle manifest has an unknown format")

    stored_trees = frozen.get("trees")
    if not isinstance(stored_trees, dict) or any(
        not isinstance(stored_trees.get(name), dict)
        or not isinstance(stored_trees[name].get("files"), dict)
        for name, _ in TREE_GLOBS
    ):
        return ManifestCheck(ok=False, message="frozen oracle manifest has no tree file maps")

    current = compute(paths)
    changes: list[TreeChange] = []
    for name, _ in TREE_GLOBS:
        old_files: dict[str, str] = stored_trees[name]["files"]
        new_files: dict[str, str] = current["trees"][name]["files"]
        for rel in sorted(set(old_files) | set(new_files)):
            if rel not in old_files:
                kind = "added"
            elif rel not in new_files:
                kind = "removed"
            elif old_files[rel] != new_files[rel]:
                kind = "modified"
            else:
                continue
            changes.append(TreeChange(tree=name, path=rel, change=kind))
    changes.sort(key=lambda c: (c.tree, c.path))

    stored_ref = frozen.get("reference")
    if not isinstance(stored_ref, dict):
        stored_ref = {}
    reference_changed = (
        stored_ref.get("image_digest") != PG_IMAGE_DIGEST
        or stored_ref.get("server_version") != PG_SERVER_VERSION
    )

    stored_suites = frozen.get("pg_suites")
    if not isinstance(stored_suites, dict):
        stored_suites = {}
    drifted_suites = [
        key
        for key in _PG_SUITE_KEYS
        if stored_suites.get(key) != current["pg_suites"][key]
    ]

    problems: list[str] = []
    if changes:
        problems.append(f"{len(changes)} file change(s) in protected/verifier/corpus trees")
    if reference_changed:
        problems.append("reference image digest or server version differs from the pin")
    if drifted_suites:
        problems.append("pg suite manifest hash differs: " + ", ".join(drifted_suites))

    ok = not problems
    message = "oracle manifest matches the working tree" if ok else "; ".join(problems)
    return ManifestCheck(
        ok=ok,
        changes=changes,
        reference_changed=reference_changed,
        message=message,
    )
