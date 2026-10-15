"""SWARM-14: verifier-mutation guard. Feature patches cannot alter oracle, expected outputs,
denominator or acceptance checks.

Lease FAC-07.

Protected sets (from pgarbor_harness.config):
  * PROTECTED_GLOBS: ``contracts/**``, ``harness/oracle/**``, ``tests/expected/**`` — changed
    ONLY through the oracle review path;
  * VERIFIER_GLOBS: the harness code + case corpus — changed only by HAR-/FAC- leases (checked
    by assemble.check_patch) or the oracle review path.

``check_campaign_preconditions(repo_root)`` — called by campaign.run before anything runs:
  1. pgarbor_harness.oracle.manifest.check(paths): any change in trees "protected", "expected",
     "cases" or "verifier" since the frozen manifest -> violation, UNLESS the change is
     covered by a review record ``runs/state/oracle_reviews.jsonl`` line
     ``{"at_unix", "reviewer", "git_sha", "paths": [...], "reason"}`` whose git_sha is an
     ancestor of HEAD and whose paths include the changed path;
  2. the denominator cannot shrink silently: compare the case count / regress test count /
     isolation test count with the previous campaign's evidence pack
     (``runs/state/last_evidence.json`` pointer); a decrease without a review record ->
     violation ``denominator_shrank``;
  3. no case file may gain a ``quarantine`` key without a review record -> violation;
  4. selftest.run_offline must be ok -> else violation ``selftest_failed``.
Returns a GuardReport; campaign refuses to start when ``ok`` is False (exit code 3) and
writes the report into the run dir. There is NO override flag.

``record_review(repo_root, reviewer, paths, reason)``: appends a review record (the oracle
review path's only write API; requires PGARBOR_ORACLE_REVIEW=1) and re-freezes the manifest.
"""

from __future__ import annotations

import json
import os
import re
import subprocess
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Optional

from pgarbor_factory import evidence as _evidence
from pgarbor_factory import gitutil
from pgarbor_harness.config import PROTECTED_GLOBS, VERIFIER_GLOBS, Paths
from pgarbor_harness.hashing import glob_match
from pgarbor_harness.oracle import manifest as _manifest
from pgarbor_harness.selftest import run as _selftest

#: Rust engine sources and build inputs (feature code; not protected, not verifier).
ENGINE_GLOBS: tuple[str, ...] = (
    "crates/**",
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain.toml",
)

#: Case corpus glob (the same tree the manifest hashes as "cases").
CASE_GLOB = "tests/cases/**/*.yaml"

#: Review ledger, relative to the repo root (Paths.state / REVIEWS_FILE).
REVIEWS_FILE = "oracle_reviews.jsonl"
REVIEWS_REL = "runs/state/oracle_reviews.jsonl"

#: Review-record env gate shared with the oracle manifest and recorder.
REVIEW_ENV = "PGARBOR_ORACLE_REVIEW"

_SHA_RE = re.compile(r"[0-9a-f]{7,64}")
#: A top-level YAML ``quarantine:`` key (column 0 only, so nested keys do not match).
_QUARANTINE_RE = re.compile(r"(?m)^quarantine\s*:")


@dataclass
class GuardViolation:
    kind: str
    path: str = ""
    detail: str = ""


@dataclass
class GuardReport:
    ok: bool
    violations: list[GuardViolation] = field(default_factory=list)

    def to_json(self) -> dict[str, Any]:
        return {"ok": self.ok, "violations": [v.__dict__ for v in self.violations]}


@dataclass(frozen=True)
class _Review:
    at_unix: int
    git_sha: str
    paths: frozenset[str]


class _Reviews:
    """Review records plus a memoized ``git merge-base --is-ancestor <sha> HEAD`` check."""

    def __init__(self, root: Path, records: list[_Review]) -> None:
        self.root = root
        self.records = records
        self._ancestor: dict[str, bool] = {}

    def is_ancestor(self, sha: str) -> bool:
        cached = self._ancestor.get(sha)
        if cached is not None:
            return cached
        ok = False
        # The regex rejects anything that could be read as a git option (leading '-').
        if _SHA_RE.fullmatch(sha):
            try:
                p = subprocess.run(
                    ["git", "merge-base", "--is-ancestor", sha, "HEAD"],
                    cwd=str(self.root), capture_output=True, check=False,
                )
                ok = p.returncode == 0
            except OSError:
                ok = False
        self._ancestor[sha] = ok
        return ok

    def covers(self, token: str, min_at: Optional[int] = None) -> bool:
        """True when a record names ``token`` in its paths, its sha is an ancestor of HEAD and
        (if ``min_at`` is given) it was written at or after ``min_at``."""
        for r in self.records:
            if token not in r.paths:
                continue
            if min_at is not None and r.at_unix < min_at:
                continue
            if self.is_ancestor(r.git_sha):
                return True
        return False


def _norm_rel(rel: str) -> Optional[str]:
    """Repo-relative POSIX path, or None when it is absolute or escapes the repo."""
    if not rel or rel.startswith("/") or "\\" in rel:
        return None
    parts = rel.split("/")
    if any(part in ("", ".", "..") for part in parts):
        return None
    return "/".join(parts)


def _parse_review(line: str) -> Optional[_Review]:
    try:
        d = json.loads(line)
    except ValueError:
        return None
    if not isinstance(d, dict):
        return None
    at = d.get("at_unix")
    sha = d.get("git_sha")
    reviewer = d.get("reviewer")
    reason = d.get("reason")
    paths = d.get("paths")
    if not isinstance(at, int) or isinstance(at, bool):
        return None
    if not isinstance(sha, str) or not sha:
        return None
    if not isinstance(reviewer, str) or not reviewer.strip():
        return None
    if not isinstance(reason, str) or not reason.strip():
        return None
    if not isinstance(paths, list) or not all(isinstance(p, str) and p for p in paths):
        return None
    return _Review(at, sha, frozenset(paths))


def _load_reviews(root: Path, violations: list[GuardViolation]) -> list[_Review]:
    """Read the review ledger. Unreadable or malformed content is a violation, never an
    exception, and a malformed line never covers anything."""
    path = Paths(root=root).state / REVIEWS_FILE
    if not path.exists():
        return []
    try:
        text = path.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError) as e:
        violations.append(GuardViolation("review_ledger_unreadable", REVIEWS_REL,
                                         f"{type(e).__name__}: {e}"))
        return []
    records: list[_Review] = []
    for lineno, line in enumerate(text.splitlines(), start=1):
        if not line.strip():
            continue
        rec = _parse_review(line)
        if rec is None:
            violations.append(GuardViolation("review_record_invalid", REVIEWS_REL,
                                             f"line {lineno} is not a valid review record"))
            continue
        records.append(rec)
    return records


def _check_manifest(paths: Paths, reviews: _Reviews,
                    violations: list[GuardViolation]) -> None:
    try:
        mc = _manifest.check(paths)
    except Exception as e:  # fail closed: a broken or missing manifest refuses the campaign
        violations.append(GuardViolation("manifest_check_failed", "",
                                         f"{type(e).__name__}: {e}"))
        return
    if mc.reference_changed:
        violations.append(GuardViolation(
            "reference_changed", "", "oracle reference identity differs from the frozen manifest"))
    if not mc.ok and not mc.changes and not mc.reference_changed:
        violations.append(GuardViolation("manifest_check_failed", "",
                                         mc.message or "manifest check not ok"))

    # Dedupe by path: a tests/expected/** file appears in both "protected" and "expected".
    by_path: dict[str, list[str]] = {}
    for ch in mc.changes:
        by_path.setdefault(ch.path, []).append(f"{ch.tree}:{ch.change}")

    for path in sorted(by_path):
        if not reviews.covers(path):
            violations.append(GuardViolation(
                "unreviewed_change", path, ", ".join(sorted(set(by_path[path])))))

    for path in sorted(by_path):
        if not glob_match(path, CASE_GLOB) or reviews.covers(path):
            continue
        file_path = paths.root / path
        if not file_path.is_file():
            continue  # removed cases carry no quarantine key
        try:
            text = file_path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError) as e:
            violations.append(GuardViolation("case_unreadable", path,
                                             f"{type(e).__name__}: {e}"))
            continue
        if _QUARANTINE_RE.search(text):
            violations.append(GuardViolation(
                "quarantine_unreviewed", path,
                "top-level quarantine key present without a review record"))


def _current_cataloged(name: str, paths: Paths) -> Optional[int]:
    """Offline item count for a family, or None when no offline count source exists."""
    if name == "differential":
        if not paths.cases.is_dir():
            return 0
        return sum(1 for p in paths.cases.rglob("*.yaml") if p.is_file())
    if name == "isolation":
        try:
            from pgarbor_harness.isolation import spec_parser

            doc = spec_parser.build_manifest(paths.pg_isolation_dir)
            return len(doc["tests"])
        except Exception:
            return None
    return None


def _check_denominator(root: Path, paths: Paths, reviews: _Reviews,
                       violations: list[GuardViolation]) -> None:
    try:
        prev = _evidence.load_last(root)
    except Exception as e:  # fail closed
        violations.append(GuardViolation("evidence_unavailable", "",
                                         f"{type(e).__name__}: {e}"))
        return
    if prev is None:
        return  # no previous campaign, nothing to shrink from
    for fam in prev.families:
        if fam.total_cataloged <= 0:
            continue
        token = f"denominator:{fam.name}"
        current = _current_cataloged(fam.name, paths)
        if current is None:
            violations.append(GuardViolation(
                "denominator_unverifiable", token,
                f"no offline count source for family {fam.name!r} "
                f"(previous total_cataloged {fam.total_cataloged})"))
            continue
        if current < fam.total_cataloged:
            # A shrink needs a review record naming this family, written after the previous
            # campaign finished.
            if reviews.covers(token, min_at=prev.finished_unix):
                continue
            violations.append(GuardViolation(
                "denominator_shrank", token,
                f"{fam.name}: total_cataloged {fam.total_cataloged} -> {current}"))


def _check_selftest(violations: list[GuardViolation]) -> None:
    try:
        report = _selftest.run_offline()
    except Exception as e:  # fail closed
        violations.append(GuardViolation("selftest_failed", "",
                                         f"run_offline raised {type(e).__name__}: {e}"))
        return
    if not report.ok:
        failed = [c.name for c in report.checks if not c.ok]
        violations.append(GuardViolation("selftest_failed", "",
                                         ", ".join(failed) or "report not ok"))


def check_campaign_preconditions(repo_root: Path, run_selftest: bool = True) -> GuardReport:
    """Run all four guard checks. There is no way to skip or waive a check: ``run_selftest``
    must stay True (False raises ValueError rather than silently dropping check 4)."""
    if not run_selftest:
        raise ValueError("the selftest cannot be skipped by the verifier guard")
    root = Path(repo_root)
    paths = Paths(root=root)
    violations: list[GuardViolation] = []
    reviews = _Reviews(root, _load_reviews(root, violations))
    _check_manifest(paths, reviews, violations)
    _check_denominator(root, paths, reviews, violations)
    _check_selftest(violations)
    return GuardReport(ok=not violations, violations=violations)


def classify_path(rel: str) -> str:
    """"protected" | "verifier" | "engine" | "other" for a repo-relative path."""
    norm = _norm_rel(rel)
    if norm is None:
        return "other"
    if any(glob_match(norm, g) for g in PROTECTED_GLOBS):
        return "protected"
    if any(glob_match(norm, g) for g in VERIFIER_GLOBS):
        return "verifier"
    if any(glob_match(norm, g) for g in ENGINE_GLOBS):
        return "engine"
    return "other"


def record_review(repo_root: Path, reviewer: str, paths: list[str], reason: str) -> None:
    """Append one review record for ``paths`` at HEAD, then re-freeze the manifest.

    Only the oracle review path may call this: it requires PGARBOR_ORACLE_REVIEW=1. The
    re-freeze reuses the ``reference`` identity already stored in the frozen manifest, so the
    manifest must exist first. Validation runs before anything is written."""
    if os.environ.get(REVIEW_ENV) != "1":
        raise PermissionError(
            "oracle review records can only be written through the oracle review path "
            f"(set {REVIEW_ENV}=1)")
    if not isinstance(reviewer, str) or not reviewer.strip():
        raise ValueError("a review record needs a reviewer")
    if not isinstance(reason, str) or not reason.strip():
        raise ValueError("a review record needs a reason")
    if not paths:
        raise ValueError("a review record needs at least one path")
    clean: set[str] = set()
    for p in paths:
        norm = _norm_rel(p) if isinstance(p, str) else None
        if norm is None:
            raise ValueError(f"bad review path: {p!r}")
        clean.add(norm)

    root = Path(repo_root)
    paths_obj = Paths(root=root)
    manifest_file = root / _manifest.MANIFEST_RELPATH
    if not manifest_file.is_file():
        raise RuntimeError("no frozen oracle manifest to re-freeze; freeze it first")
    try:
        frozen = json.loads(manifest_file.read_text(encoding="utf-8"))
        reference = frozen["reference"]
    except (OSError, ValueError, KeyError, TypeError) as e:
        raise RuntimeError(f"frozen oracle manifest has no usable reference: {e}") from e
    if not isinstance(reference, dict):
        raise RuntimeError("frozen oracle manifest reference is not an object")

    record = {
        "at_unix": int(time.time()),
        "reviewer": reviewer,
        "git_sha": gitutil.head_sha(root),
        "paths": sorted(clean),
        "reason": reason,
    }
    paths_obj.state.mkdir(parents=True, exist_ok=True)
    with open(paths_obj.state / REVIEWS_FILE, "a", encoding="utf-8") as f:
        f.write(json.dumps(record, sort_keys=True, ensure_ascii=False) + "\n")
    _manifest.freeze(paths_obj, reference)
