"""ORACLE-04: map (reference, target, diffs) to exactly one model.Outcome.

Lease HAR-14.

Algorithm (``classify``) — first matching rule wins:

 1. any target step has ``harness_error``                    -> HARNESS_ERROR
    (reason = "harness:" + first 80 chars)
 2. CORRUPTION: any target error result with sqlstate ``XX001`` (data_corrupted) or ``XX002``
    (index_corrupted) whose reference counterpart (same step/result index) is not an error
    with the same sqlstate; or ``crash_evidence`` contains "corrupt" (case-insensitive)
                                                             -> CORRUPTION
 3. CRASH: any target step ``connection_lost`` whose reference step is not; or any target
    error with sqlstate ``XX000`` (internal_error — a caught Rust panic or invariant failure)
    whose reference counterpart is not XX000; or ``crash_evidence`` is non-empty
                                                             -> CRASH
 4. HANG: any target step ``timed_out`` whose reference step did not   -> HANG
 5. no diffs                                                   -> PASS
 6. Let ``d`` = diffs[0] (first divergence). Locate the reference and target results at d's
    step/result (if any):
      a. target result is an error with sqlstate ``0A000`` (feature_not_supported, which
         includes every ``PgError::unimplemented``) and the reference result is not an
         identical 0A000 error                                -> UNSUPPORTED
      b. reference result is an error and the target result is not (kind rows/command/
         empty/description/copy_out)                          -> UNEXPECTED_ACCEPTANCE
      c. otherwise                                            -> MISMATCH
    reason = "first_divergence".

FLAKY is decided by runner.differential (rerun), INVALID_CASE/BLOCKED before execution;
classify never returns them.

Signature (``CaseResult.signature``, used by pgarbor_factory.cluster), all strings:
  ``diff_kind`` = d.kind (or the rule name for 1-4: "harness"/"corruption"/"crash"/"hang"),
  ``sqlstate`` = target error sqlstate at d ("" if none), ``ref_sqlstate`` likewise,
  ``message_template`` = sqltext.message_template(target error message) or of the reference
  message for UNEXPECTED_ACCEPTANCE, ``step_kind`` = case.steps[i].kind ("setup" for setup),
  ``first_keyword`` = sqltext.first_keyword of the step's SQL (for the statement that produced
  the divergent result when statements can be split, else the whole SQL),
  ``unimplemented_lease`` = the lease id when the target error at d is SQLSTATE 0A000 with
  DETAIL matching ``^pgarbor-unimplemented: ([A-Z][A-Z0-9]{1,9}-\\d{2,3})$`` (the exact
  shape PgError::unimplemented produces, crates/pgarbor-common/src/error.rs), else "".

Hard invariants (asserted by selftest): UNSUPPORTED, BLOCKED, FLAKY, INVALID_CASE and every
non-PASS outcome never satisfy ``Outcome.is_pass``; a case with any diff is never PASS.
"""

from __future__ import annotations

from typing import Optional

from ..cases.model import Case
from ..model import CaseResult, Diff, Transcript


def classify(case: Case, reference: Transcript, target: Transcript, diffs: list[Diff],
             crash_evidence: Optional[str] = None, duration_ms: int = 0) -> CaseResult:
    """Return a fully populated CaseResult (outcome, reason, first_divergence=diffs[0] if any,
    diffs, feature_ids=case.feature_ids, signature, duration_ms, family=case.family)."""
    raise NotImplementedError("HAR-14: outcome classification")


def locate(diff: Diff) -> tuple[str, int, Optional[int]]:
    """Parse a Diff.path into (section "setup"|"steps", step index, result index or None).
    ``steps[3].results[0].rows[1][2]`` -> ("steps", 3, 0); ``setup[0]`` -> ("setup", 0, None)."""
    raise NotImplementedError("HAR-14: parse diff path")
