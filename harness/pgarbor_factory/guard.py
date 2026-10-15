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

from dataclasses import dataclass, field
from pathlib import Path
from typing import Any


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


def check_campaign_preconditions(repo_root: Path, run_selftest: bool = True) -> GuardReport:
    raise NotImplementedError("FAC-07: guard preconditions")


def classify_path(rel: str) -> str:
    """"protected" | "verifier" | "engine" | "other" for a repo-relative path."""
    raise NotImplementedError("FAC-07: path classification")


def record_review(repo_root: Path, reviewer: str, paths: list[str], reason: str) -> None:
    raise NotImplementedError("FAC-07: record oracle review")
