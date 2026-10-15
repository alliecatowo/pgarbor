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

from dataclasses import dataclass, field
from enum import Enum
from pathlib import Path
from typing import Iterable, Optional

from ..cases.model import Case
from ..model import ReferenceIdentity, Transcript
from .pool import OraclePool


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


def check_intent(case: Case, transcript: Transcript) -> list[str]:
    """Return human-readable intent violations, e.g.
    ``"step 3: expect=error sqlstate=23505 but reference returned kind=command"``."""
    raise NotImplementedError("HAR-04: author intent check")


def record_case(pool: OraclePool, case: Case, identity: ReferenceIdentity, expected_root: Path,
                allow_overwrite: bool = False) -> RecordResult:
    """Record one case per the module algorithm."""
    raise NotImplementedError("HAR-04: record one case")


def record_corpus(pool: OraclePool, cases: Iterable[Case], expected_root: Path,
                  allow_overwrite: bool = False, dry_run: bool = False,
                  jobs: int = 4) -> list[RecordResult]:
    """Record many cases with a thread pool of ``jobs`` workers (each uses its own per-case db).

    Steps: verify oracle fingerprint once (oracle.fingerprint.verify_live) to build the
    ReferenceIdentity; enforce the PGARBOR_ORACLE_REVIEW gate described in the module
    docstring; results returned sorted by case_id. Cases with ``restart`` steps are recorded
    sequentially after all others (they restart the shared container)."""
    raise NotImplementedError("HAR-04: record corpus")
