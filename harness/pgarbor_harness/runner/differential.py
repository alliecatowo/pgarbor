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

import copy
import dataclasses
import json
import threading
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path, PurePosixPath
from typing import Iterable, Optional

from .. import config
from ..cases.loader import InvalidCase
from ..cases.model import Case
from ..compare import classify as classify_mod
from ..compare import typed as typed_mod
from ..model import (
    CaseResult,
    ExpectedFile,
    Outcome,
    ReferenceIdentity,
    Transcript,
    canonical_json,
    expected_path,
    write_canonical_json,
)
from . import executor as case_executor
from .targets import Target, has_capabilities

#: Serializes appends to results.jsonl across worker threads.
_RESULTS_LOCK = threading.Lock()

#: Outcomes that are never rerun (step 6).
_NO_RERUN: tuple[Outcome, ...] = (
    Outcome.PASS, Outcome.CRASH, Outcome.CORRUPTION, Outcome.HARNESS_ERROR,
)


def _result(case: Case, outcome: Outcome, reason: str = "") -> CaseResult:
    return CaseResult(case_id=case.id, family=case.family, outcome=outcome, reason=reason,
                      feature_ids=list(case.feature_ids))


def _harness_error(case: Case, exc: BaseException) -> CaseResult:
    return _result(case, Outcome.HARNESS_ERROR, "exception:" + type(exc).__name__)


def _zeroed_json(transcript: Transcript) -> str:
    """Canonical JSON of a copy of the transcript with every elapsed_ms set to 0."""
    return canonical_json(copy.deepcopy(transcript).zero_timings())


def _display_path(path: Path) -> str:
    """Repo-relative POSIX path when ``path`` is under the repo, else absolute POSIX."""
    resolved = path.resolve()
    root = config.Paths().root.resolve()
    try:
        return resolved.relative_to(root).as_posix()
    except ValueError:
        return resolved.as_posix()


def _append_result(out_dir: Path, result: CaseResult) -> None:
    """Step 8: one compact JSON line appended under the module lock."""
    line = json.dumps(result.to_json(), sort_keys=True, ensure_ascii=False,
                      separators=(",", ":")) + "\n"
    with _RESULTS_LOCK:
        out_dir.mkdir(parents=True, exist_ok=True)
        with open(out_dir / "results.jsonl", "a", encoding="utf-8", newline="\n") as f:
            f.write(line)


def _execute(case: Case, target: Target, expected_root: Path, out_dir: Path,
             live_reference: Optional[ReferenceIdentity], reruns: int) -> CaseResult:
    """Steps 1-7. Any exception raised here is converted to HARNESS_ERROR by run_case."""
    # Step 1: quarantine.
    if case.quarantine is not None:
        return _result(case, Outcome.INVALID_CASE, "quarantined")

    # Step 2: the reference must exist, match the case bytes and the live oracle identity.
    ref_file = expected_path(expected_root, case.id)
    if not ref_file.is_file():
        return _result(case, Outcome.BLOCKED, "reference_missing")
    expected = ExpectedFile.load(ref_file)
    if expected.case_sha256 != case.sha256:
        return _result(case, Outcome.BLOCKED, "reference_stale")
    # ExpectedFile documents that all three identity fields must match the live oracle; the
    # fingerprint is the one the spec names, the other two are checked for the same reason.
    if live_reference is not None and expected.reference != live_reference:
        return _result(case, Outcome.BLOCKED, "reference_stale")

    # Step 3: capabilities.
    missing = has_capabilities(target, case.required_capabilities)
    if missing:
        return _result(case, Outcome.BLOCKED, "capability:" + missing[0])

    # Steps 4-5: execute once, compare, classify.
    observed = case_executor.execute_case(case, target, "target", attempt=1)
    diffs = typed_mod.compare_transcripts(case, expected.transcript, observed)
    result = classify_mod.classify(case, expected.transcript, observed, diffs)

    # Step 6: rerun policy for non-terminal outcomes.
    attempts = 1
    if result.outcome not in _NO_RERUN and reruns > 0:
        rerun = case_executor.execute_case(case, target, "target", attempt=2)
        attempts = 2
        if _zeroed_json(observed) != _zeroed_json(rerun):
            result = dataclasses.replace(result, outcome=Outcome.FLAKY,
                                         reason="target_nondeterministic")
    result = dataclasses.replace(result, attempts=attempts)

    # Step 7: the observed transcript, canonical, under out_dir/transcripts.
    transcript_file = out_dir / "transcripts" / f"{case.id}.json"
    write_canonical_json(transcript_file, observed)
    return dataclasses.replace(result, transcript_path=_display_path(transcript_file))


def run_case(case: Case, target: Target, expected_root: Path, out_dir: Path,
             live_reference: Optional[ReferenceIdentity] = None, reruns: int = 1) -> CaseResult:
    """Steps 1-7 for one case."""
    expected_root = Path(expected_root)
    out_dir = Path(out_dir)
    started = time.monotonic()
    try:
        result = _execute(case, target, expected_root, out_dir, live_reference, reruns)
    except Exception as exc:  # one broken case must not stop the run
        result = _harness_error(case, exc)
    result = dataclasses.replace(result, duration_ms=int((time.monotonic() - started) * 1000))
    _append_result(out_dir, result)  # step 8
    return result


def _run_parallel(cases: list[Case], target: Target, expected_root: Path, out_dir: Path,
                  live_reference: Optional[ReferenceIdentity], jobs: int,
                  reruns: int) -> list[CaseResult]:
    if not cases:
        return []
    results: list[CaseResult] = []
    pool = ThreadPoolExecutor(max_workers=max(1, jobs), thread_name_prefix="pgarbor-diff")
    futures = [pool.submit(run_case, case, target, expected_root, out_dir, live_reference,
                           reruns) for case in cases]
    try:
        for fut in as_completed(futures):
            results.append(fut.result())
    except BaseException:
        # KeyboardInterrupt (or any abort): schedule nothing new, wait for running cases.
        pool.shutdown(wait=True, cancel_futures=True)
        raise
    pool.shutdown(wait=True)
    return results


def run_cases(cases: Iterable[Case], target: Target, expected_root: Path, out_dir: Path,
              live_reference: Optional[ReferenceIdentity] = None, jobs: int = 4,
              reruns: int = 1) -> list[CaseResult]:
    """Run many cases per module docstring; ``target.start()`` first, ``target.stop()`` last."""
    selected = list(cases)
    parallel = [case for case in selected if not case.needs_restart]
    serial = [case for case in selected if case.needs_restart]
    expected_root = Path(expected_root)
    out_dir = Path(out_dir)

    target.start()
    results: list[CaseResult] = []
    try:
        results.extend(_run_parallel(parallel, target, expected_root, out_dir, live_reference,
                                     jobs, reruns))
        for case in serial:
            results.append(run_case(case, target, expected_root, out_dir, live_reference,
                                    reruns))
    finally:
        target.stop()
    results.sort(key=lambda r: r.case_id)
    return results


def _family_of(path: str) -> str:
    """First directory under a ``cases`` directory (``tests/cases/<family>/x.yaml``)."""
    parts = PurePosixPath(path.replace("\\", "/")).parts
    for i, part in enumerate(parts):
        if part == "cases" and i + 2 < len(parts):
            return parts[i + 1]
    return ""


def invalid_results(invalid: Iterable[InvalidCase]) -> list[CaseResult]:
    """InvalidCase -> CaseResult(INVALID_CASE). case_id = parsed id or the file path; family =
    first directory under tests/cases."""
    out: list[CaseResult] = []
    for item in invalid:
        first = item.errors[0] if item.errors else "unknown"
        out.append(CaseResult(
            case_id=item.case_id or item.path,
            family=_family_of(item.path),
            outcome=Outcome.INVALID_CASE,
            reason="schema:" + first,
        ))
    out.sort(key=lambda r: r.case_id)
    return out
