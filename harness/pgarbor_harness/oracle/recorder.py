"""ORACLE-02: the trusted reference recorder. The ONLY writer of ``tests/expected/**``.

Lease HAR-04.

Algorithm for one case (``record_case``):
  1. skip quarantined cases (return RecordStatus.SKIPPED_QUARANTINED);
  2. run the case TWICE against the pooled oracle (runner.executor.execute_case with a
     ``PostgresTarget``), each in a fresh per-case database (attempt=1, attempt=2);
  3. zero timings (Transcript.zero_timings) and apply the case's declared normalizers
     (compare.normalize.apply_for_recording) to both;
  4. if the two canonical JSON transcripts differ -> RecordStatus.NONDETERMINISTIC, nothing
     written; the case must be fixed or quarantined (reported INVALID_CASE
     reason="reference_nondeterministic");
  5. check author intent (``check_intent``): every step's ``expect``/``sqlstate``/
     ``expect_blocked`` must agree with the recording; violations -> RecordStatus.INTENT_MISMATCH,
     nothing written (INVALID_CASE reason="intent_mismatch");
  6. any harness_error/timed_out/connection_lost in the recording -> RecordStatus.ORACLE_ERROR;
  7. otherwise write ``ExpectedFile(case_id, case.sha256, identity, transcript)`` to
     ``model.expected_path(paths.expected, case.id)`` with ``ExpectedFile.save`` (canonical
     JSON, atomic). If the file exists and is byte-identical -> UNCHANGED; if it exists and
     differs -> written only when ``allow_overwrite`` (the oracle review path passes
     ``--rerecord``), else RecordStatus.WOULD_CHANGE and the diff is returned.

Intent rules (``check_intent``), per step with a non-"any" expect, looking at the step's
results (for async steps: the matching ``wait`` step carries the results):
  * ``ok``: no result has kind "error" (rows or command both fine)
  * ``rows``: last result kind == "rows"
  * ``error``: some result has kind "error"; if ``sqlstate`` given it must equal that
    result's error.sqlstate
  * ``notice``: at least one notice received during the step
  * ``expect_blocked``: equals ``blocked_observed``.

Recording is never done by feature workers; ``record_corpus`` refuses to run unless the
environment variable ``PGARBOR_ORACLE_REVIEW=1`` is set or ``dry_run`` is True (dry run
records into a temp dir and reports what would change).
"""

from __future__ import annotations

import difflib
import os
import tempfile
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, field
from enum import Enum
from pathlib import Path
from typing import Callable, Iterable, Optional

from ..cases.model import Case, Step
from ..compare.normalize import apply_case_normalizers
from ..model import (
    ExpectedFile,
    ReferenceIdentity,
    StatementResult,
    StepObservation,
    Transcript,
    canonical_json,
    expected_path,
)
from ..runner.executor import execute_case
from ..runner.targets import PostgresTarget
from .fingerprint import verify_live
from .pool import OraclePool

#: Environment variable that opens the oracle review path (recording, manifest freeze, ...).
REVIEW_ENV = "PGARBOR_ORACLE_REVIEW"


class RecordStatus(str, Enum):
    WRITTEN = "WRITTEN"
    UNCHANGED = "UNCHANGED"
    WOULD_CHANGE = "WOULD_CHANGE"
    NONDETERMINISTIC = "NONDETERMINISTIC"
    INTENT_MISMATCH = "INTENT_MISMATCH"
    ORACLE_ERROR = "ORACLE_ERROR"
    SKIPPED_QUARANTINED = "SKIPPED_QUARANTINED"


@dataclass
class RecordResult:
    case_id: str
    status: RecordStatus
    path: Optional[str] = None
    messages: list[str] = field(default_factory=list)


# ---------------------------------------------------------------------------------------------
# Intent checking
# ---------------------------------------------------------------------------------------------


def _faulted(obs: StepObservation) -> bool:
    """Observations with a harness/oracle fault are reported as ORACLE_ERROR (step 6), not as
    intent violations, so their (missing or partial) results are not judged here."""
    return obs.timed_out or obs.connection_lost or obs.harness_error is not None


def _kinds(results: list[StatementResult]) -> str:
    return ",".join(r.kind for r in results) if results else "none"


def _sqlstates(results: list[StatementResult]) -> str:
    states = [(r.error.sqlstate if r.error is not None else "none")
              for r in results if r.kind == "error"]
    return ",".join(states) if states else "none"


def _jb(value: Optional[bool]) -> str:
    if value is None:
        return "null"
    return "true" if value else "false"


def _carrier_index(case: Case, step: Step) -> int:
    """Index of the step whose observation carries the results of ``step``: the step itself,
    or for an async step the first later ``wait`` on the same session. -1 if none exists."""
    if not step.is_async:
        return step.index
    for later in case.steps:
        if later.index > step.index and later.kind == "wait" and later.session == step.session:
            return later.index
    return -1


def _check_expect(step: Step, obs: StepObservation) -> Optional[str]:
    """Return a violation message for a non-"any" expect, or None if the recording agrees."""
    results = obs.results
    errors = [r for r in results if r.kind == "error"]
    prefix = f"step {step.index}: expect={step.expect}"
    if step.expect == "ok":
        if errors:
            return f"{prefix} but reference returned kind=error sqlstate={_sqlstates(errors)}"
        return None
    if step.expect == "rows":
        if not results or results[-1].kind != "rows":
            return f"{prefix} but reference returned kind={_kinds(results)}"
        return None
    if step.expect == "error":
        if step.sqlstate is not None:
            prefix = f"{prefix} sqlstate={step.sqlstate}"
        if not errors:
            return f"{prefix} but reference returned kind={_kinds(results)}"
        if step.sqlstate is not None and not any(
                r.error is not None and r.error.sqlstate == step.sqlstate for r in errors):
            return f"{prefix} but reference returned sqlstate={_sqlstates(errors)}"
        return None
    if step.expect == "notice":
        if not obs.notices:
            return f"{prefix} but reference received no notice"
        return None
    return f"step {step.index}: unknown expect={step.expect!r}"


def check_intent(case: Case, transcript: Transcript) -> list[str]:
    """Return human-readable intent violations, e.g.
    ``"step 3: expect=error sqlstate=23505 but reference returned kind=command"``."""
    by_index = {obs.index: obs for obs in transcript.steps}
    violations: list[str] = []
    for step in case.steps:
        own = by_index.get(step.index)
        if step.expect_blocked is not None and own is not None and not _faulted(own):
            if own.blocked_observed != step.expect_blocked:
                violations.append(
                    f"step {step.index}: expect_blocked={_jb(step.expect_blocked)} "
                    f"but reference blocked_observed={_jb(own.blocked_observed)}")
        if step.expect == "any":
            continue
        carrier = _carrier_index(case, step)
        if carrier < 0:
            violations.append(
                f"step {step.index}: expect={step.expect} but async step has no matching wait")
            continue
        obs = by_index.get(carrier)
        if obs is None or _faulted(obs):
            continue  # not observed / faulted: reported as ORACLE_ERROR instead
        message = _check_expect(step, obs)
        if message is not None:
            violations.append(message)
    return violations


# ---------------------------------------------------------------------------------------------
# Recording one case
# ---------------------------------------------------------------------------------------------


def _oracle_faults(case: Case, transcript: Transcript) -> list[str]:
    """Reasons the recording is unusable (step 6 of the module algorithm)."""
    faults: list[str] = []
    for obs in transcript.setup:
        if obs.harness_error is not None:
            faults.append(f"setup: harness_error: {obs.harness_error}")
        if obs.timed_out:
            faults.append("setup: timed out on the reference")
        if obs.connection_lost:
            faults.append("setup: reference connection lost")
    for obs in transcript.steps:
        where = f"step {obs.index}"
        if obs.harness_error is not None:
            faults.append(f"{where}: harness_error: {obs.harness_error}")
        if obs.timed_out:
            faults.append(f"{where}: timed out on the reference")
        if obs.connection_lost:
            faults.append(f"{where}: reference connection lost")
    if len(transcript.steps) != len(case.steps):
        faults.append(f"reference transcript has {len(transcript.steps)} of "
                      f"{len(case.steps)} steps")
    return faults


def _diff(old: bytes, new: bytes, name: str) -> list[str]:
    """Unified diff of two canonical JSON texts (without trailing newlines)."""
    return list(difflib.unified_diff(
        old.decode("utf-8", "replace").splitlines(),
        new.decode("utf-8", "replace").splitlines(),
        fromfile=name, tofile=name, lineterm=""))


def _record_one(pool: OraclePool, case: Case, identity: ReferenceIdentity,
                expected_root: Path, allow_overwrite: bool) -> RecordResult:
    """The module algorithm for one case, without the review gate."""
    if case.quarantine is not None:
        reason = case.quarantine.get("reason", "")
        return RecordResult(case.id, RecordStatus.SKIPPED_QUARANTINED,
                            messages=[f"quarantined: {reason}" if reason else "quarantined"])

    target = PostgresTarget(pool)
    try:
        first = execute_case(case, target, endpoint_label="reference", attempt=1)
        second = execute_case(case, target, endpoint_label="reference", attempt=2)
    except Exception as exc:  # harness-side failure: the case cannot be recorded
        return RecordResult(case.id, RecordStatus.ORACLE_ERROR,
                            messages=[f"execution failed: {type(exc).__name__}: {exc}"])

    first = apply_case_normalizers(case, first.zero_timings())
    second = apply_case_normalizers(case, second.zero_timings())

    first_text = canonical_json(first).encode("utf-8")
    second_text = canonical_json(second).encode("utf-8")
    if first_text != second_text:
        return RecordResult(
            case.id, RecordStatus.NONDETERMINISTIC,
            messages=["reference transcripts differ between attempt 1 and attempt 2"]
            + _diff(first_text, second_text, "attempt-1 vs attempt-2"))

    violations = check_intent(case, first)
    if violations:
        return RecordResult(case.id, RecordStatus.INTENT_MISMATCH, messages=violations)

    faults = _oracle_faults(case, first)
    if faults:
        return RecordResult(case.id, RecordStatus.ORACLE_ERROR, messages=faults)

    path = expected_path(expected_root, case.id)
    expected = ExpectedFile(case.id, case.sha256, identity, first)
    new_bytes = canonical_json(expected).encode("utf-8")
    if path.exists():
        old_bytes = path.read_bytes()
        if old_bytes == new_bytes:
            return RecordResult(case.id, RecordStatus.UNCHANGED, path.as_posix())
        diff = _diff(old_bytes, new_bytes, path.as_posix())
        if not allow_overwrite:
            return RecordResult(case.id, RecordStatus.WOULD_CHANGE, path.as_posix(), diff)
        expected.save(path)
        return RecordResult(case.id, RecordStatus.WRITTEN, path.as_posix(), diff)
    expected.save(path)
    return RecordResult(case.id, RecordStatus.WRITTEN, path.as_posix())


def record_case(pool: OraclePool, case: Case, identity: ReferenceIdentity, expected_root: Path,
                allow_overwrite: bool = False) -> RecordResult:
    """Record one case per the module algorithm."""
    try:
        return _record_one(pool, case, identity, expected_root, allow_overwrite)
    except Exception as exc:  # filesystem or serialization failure
        return RecordResult(case.id, RecordStatus.ORACLE_ERROR,
                            messages=[f"record failed: {type(exc).__name__}: {exc}"])


# ---------------------------------------------------------------------------------------------
# Recording a corpus
# ---------------------------------------------------------------------------------------------


def _compare_with_real(result: RecordResult, scratch_root: Path,
                       expected_root: Path) -> RecordResult:
    """Dry run: translate a WRITTEN scratch recording into what the real tree would see."""
    if result.status is not RecordStatus.WRITTEN:
        return result
    real = expected_path(expected_root, result.case_id)
    new_bytes = expected_path(scratch_root, result.case_id).read_bytes()
    if real.exists():
        old_bytes = real.read_bytes()
        if old_bytes == new_bytes:
            return RecordResult(result.case_id, RecordStatus.UNCHANGED, real.as_posix())
        diff = _diff(old_bytes, new_bytes, real.as_posix())
    else:
        diff = _diff(b"", new_bytes, real.as_posix())
    return RecordResult(result.case_id, RecordStatus.WOULD_CHANGE, real.as_posix(), diff)


def _run_all(work: Callable[[Case], RecordResult], cases: list[Case],
             jobs: int) -> list[RecordResult]:
    """Non-restart cases on a thread pool; restart cases one at a time afterwards (they
    restart the shared container). Results sorted by case id."""
    normal = [c for c in cases if not c.runs_serially]
    restarts = [c for c in cases if c.runs_serially]
    with ThreadPoolExecutor(max_workers=max(1, jobs)) as pool_exec:
        results = list(pool_exec.map(work, normal))
    results.extend(work(c) for c in restarts)
    return sorted(results, key=lambda r: r.case_id)


def record_corpus(pool: OraclePool, cases: Iterable[Case], expected_root: Path,
                  allow_overwrite: bool = False, dry_run: bool = False,
                  jobs: int = 4) -> list[RecordResult]:
    """Record many cases with a thread pool of ``jobs`` workers (each uses its own per-case db).

    Steps: verify oracle fingerprint once (oracle.fingerprint.verify_live) to build the
    ReferenceIdentity; enforce the PGARBOR_ORACLE_REVIEW gate described in the module
    docstring; results returned sorted by case_id. Cases with ``restart`` steps are recorded
    sequentially after all others (they restart the shared container)."""
    if not dry_run and os.environ.get(REVIEW_ENV) != "1":
        raise PermissionError(
            f"recording the oracle corpus requires {REVIEW_ENV}=1 (the oracle review path) "
            "or dry_run=True")
    case_list = list(cases)
    pool.start()
    doc, fingerprint = verify_live(pool.container)
    identity = ReferenceIdentity(str(doc["image_digest"]), str(doc["server_version"]),
                                 fingerprint)

    if not dry_run:
        def work(case: Case) -> RecordResult:
            return record_case(pool, case, identity, expected_root, allow_overwrite)

        return _run_all(work, case_list, jobs)

    with tempfile.TemporaryDirectory(prefix="pgarbor-record-dry-") as tmp:
        scratch = Path(tmp)

        def dry_work(case: Case) -> RecordResult:
            scratch_result = record_case(pool, case, identity, scratch, allow_overwrite=True)
            return _compare_with_real(scratch_result, scratch, expected_root)

        return _run_all(dry_work, case_list, jobs)
