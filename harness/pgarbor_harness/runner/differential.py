"""ORACLE-02/04: the differential driver: corpus x target -> CaseResults.

Lease HAR-30.

For each selected case (thread pool of ``jobs``; cases with ``restart`` steps run serially at
the end; results sorted by case_id):

  1. quarantined -> CaseResult(INVALID_CASE, reason="quarantined") (not executed);
  2. expected file = model.expected_path(expected_root, case.id):
       missing -> BLOCKED(reason="reference_missing");
       ``case_sha256`` != case.sha256 -> BLOCKED(reason="reference_stale");
       reference identity fingerprint != ``live_reference.fingerprint_sha256`` (when given)
         -> BLOCKED(reason="reference_stale");
  3. missing capabilities (targets.has_capabilities) -> BLOCKED(reason="capability:<first>");
  4. ``observed = executor.execute_case(case, target, "target", attempt=1)``;
  5. ``diffs = compare.typed.compare_transcripts(case, expected.transcript, observed)``;
     ``result = compare.classify.classify(case, expected.transcript, observed, diffs)``;
  6. rerun policy: if result.outcome not in (PASS, CRASH, CORRUPTION, HARNESS_ERROR) and
     ``reruns > 0``: execute again (attempt=2); if canonical_json of the two observed
     transcripts (timings zeroed) differ -> outcome FLAKY(reason="target_nondeterministic"),
     ``attempts=2``; else keep the first classification with attempts=2;
  7. write the observed transcript to ``<out_dir>/transcripts/<case_id>.json`` (canonical) and
     set ``CaseResult.transcript_path`` (repo-relative when under the repo);
  8. append ``result.to_json()`` as one line to ``<out_dir>/results.jsonl`` (opened in append
     mode under a lock; lines are written in completion order, the returned list is sorted).

Invalid corpus files (cases.loader.InvalidCase) are converted by ``invalid_results`` into
INVALID_CASE results with reason ``"schema:" + first error``.

Exceptions in steps 4-6 -> HARNESS_ERROR with reason ``"exception:" + type name``; the run
continues with the next case. KeyboardInterrupt stops scheduling new cases, waits for running
ones, and re-raises.
"""

from __future__ import annotations

from pathlib import Path
from typing import Iterable, Optional

from ..cases.loader import InvalidCase
from ..cases.model import Case
from ..model import CaseResult, ReferenceIdentity
from .targets import Target


def run_case(case: Case, target: Target, expected_root: Path, out_dir: Path,
             live_reference: Optional[ReferenceIdentity] = None, reruns: int = 1) -> CaseResult:
    """Steps 1-7 for one case."""
    raise NotImplementedError("HAR-30: run one differential case")


def run_cases(cases: Iterable[Case], target: Target, expected_root: Path, out_dir: Path,
              live_reference: Optional[ReferenceIdentity] = None, jobs: int = 4,
              reruns: int = 1) -> list[CaseResult]:
    """Run many cases per module docstring; ``target.start()`` first, ``target.stop()`` last."""
    raise NotImplementedError("HAR-30: run corpus differentially")


def invalid_results(invalid: Iterable[InvalidCase]) -> list[CaseResult]:
    """InvalidCase -> CaseResult(INVALID_CASE). case_id = parsed id or the file path; family =
    first directory under tests/cases."""
    raise NotImplementedError("HAR-30: invalid case results")
