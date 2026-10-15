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
  ``diff_kind`` = d.kind (or the rule name for 1-4: "harness"/"corruption"/"crash"/"hang";
  "" when there is no divergence to name),
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

import re
from typing import Optional

from .. import sqltext
from ..cases.model import ASYNC_KINDS, Case
from ..model import CaseResult, Diff, Outcome, StatementResult, StepObservation, Transcript

_LOCATE_RE = re.compile(r"^(setup|steps)\[(\d+)\](?:\.results\[(\d+)\])?")
_UNIMPLEMENTED_DETAIL_RE = re.compile(r"pgarbor-unimplemented: ([A-Z][A-Z0-9]{1,9}-\d{2,3})")
_CORRUPTION_STATES = frozenset({"XX001", "XX002"})
_SUCCESS_KINDS = frozenset({"rows", "command", "empty", "description", "copy_out"})
_FEATURE_NOT_SUPPORTED = "0A000"
_INTERNAL_ERROR = "XX000"


def classify(case: Case, reference: Transcript, target: Transcript, diffs: list[Diff],
             crash_evidence: Optional[str] = None, duration_ms: int = 0) -> CaseResult:
    """Return a fully populated CaseResult (outcome, reason, first_divergence=diffs[0] if any,
    diffs, feature_ids=case.feature_ids, signature, duration_ms, family=case.family)."""
    diffs = list(diffs)
    d: Optional[Diff] = diffs[0] if diffs else None
    outcome, reason, rule_kind = _decide(reference, target, diffs, crash_evidence)
    signature = _signature(case, reference, target, d, outcome, rule_kind)
    return CaseResult(
        case_id=case.id,
        family=case.family,
        outcome=outcome,
        reason=reason,
        first_divergence=d,
        diffs=diffs,
        feature_ids=list(case.feature_ids),
        signature=signature,
        duration_ms=duration_ms,
    )


def locate(diff: Diff) -> tuple[str, int, Optional[int]]:
    """Parse a Diff.path into (section "setup"|"steps", step index, result index or None).
    ``steps[3].results[0].rows[1][2]`` -> ("steps", 3, 0); ``setup[0]`` -> ("setup", 0, None)."""
    parsed = _parse_path(diff.path)
    if parsed is None:
        raise ValueError(f"unparseable diff path: {diff.path!r}")
    return parsed


# ---------------------------------------------------------------------------------------------
# Private helpers
# ---------------------------------------------------------------------------------------------

_Location = tuple[str, int, Optional[int]]


def _parse_path(path: str) -> Optional[_Location]:
    m = _LOCATE_RE.match(path)
    if m is None:
        return None
    result_index = int(m.group(3)) if m.group(3) is not None else None
    return m.group(1), int(m.group(2)), result_index


def _step_list(t: Transcript, section: str) -> list[StepObservation]:
    return t.setup if section == "setup" else t.steps


def _step_at(t: Transcript, section: str, index: int) -> Optional[StepObservation]:
    steps = _step_list(t, section)
    if 0 <= index < len(steps):
        return steps[index]
    return None


def _result_at(step: Optional[StepObservation], k: Optional[int]) -> Optional[StatementResult]:
    if step is None or k is None:
        return None
    if 0 <= k < len(step.results):
        return step.results[k]
    return None


def _error_of(r: Optional[StatementResult]):
    """The ErrorInfo of a result, or None when the result is absent or not an error."""
    if r is None:
        return None
    return r.error


def _iter_target_steps(t: Transcript):
    for i, step in enumerate(t.setup):
        yield "setup", i, step
    for i, step in enumerate(t.steps):
        yield "steps", i, step


def _decide(reference: Transcript, target: Transcript, diffs: list[Diff],
            crash_evidence: Optional[str]) -> tuple[Outcome, str, str]:
    """Return (outcome, reason, rule_kind). rule_kind names the override rule for 1-4 and is
    the first divergence's kind for rule 6; "" when there is nothing to name."""
    # 1. HARNESS_ERROR
    for _section, _i, step in _iter_target_steps(target):
        if step.harness_error is not None:
            return Outcome.HARNESS_ERROR, "harness:" + step.harness_error[:80], "harness"

    # 2. CORRUPTION
    for section, i, step in _iter_target_steps(target):
        ref_step = _step_at(reference, section, i)
        for k, r in enumerate(step.results):
            err = _error_of(r)
            if err is None or err.sqlstate not in _CORRUPTION_STATES:
                continue
            ref_err = _error_of(_result_at(ref_step, k))
            if ref_err is None or ref_err.sqlstate != err.sqlstate:
                return Outcome.CORRUPTION, "corruption:" + err.sqlstate, "corruption"
    if crash_evidence and "corrupt" in crash_evidence.lower():
        return Outcome.CORRUPTION, "corruption:evidence", "corruption"

    # 3. CRASH
    for section, i, step in _iter_target_steps(target):
        ref_step = _step_at(reference, section, i)
        if step.connection_lost and not (ref_step is not None and ref_step.connection_lost):
            return Outcome.CRASH, "crash:connection_lost", "crash"
        for k, r in enumerate(step.results):
            err = _error_of(r)
            if err is None or err.sqlstate != _INTERNAL_ERROR:
                continue
            ref_err = _error_of(_result_at(ref_step, k))
            if ref_err is None or ref_err.sqlstate != _INTERNAL_ERROR:
                return Outcome.CRASH, "crash:internal_error", "crash"
    if crash_evidence:
        return Outcome.CRASH, "crash:evidence", "crash"

    # 4. HANG
    for section, i, step in _iter_target_steps(target):
        ref_step = _step_at(reference, section, i)
        if step.timed_out and not (ref_step is not None and ref_step.timed_out):
            return Outcome.HANG, "hang:timeout", "hang"

    # 5. PASS
    if not diffs:
        return Outcome.PASS, "", ""

    # 6. first divergence decides
    d = diffs[0]
    tgt_res: Optional[StatementResult] = None
    ref_res: Optional[StatementResult] = None
    parsed = _parse_path(d.path)
    if parsed is not None:
        section, i, k = parsed
        tgt_res = _result_at(_step_at(target, section, i), k)
        ref_res = _result_at(_step_at(reference, section, i), k)
    tgt_err = _error_of(tgt_res)
    ref_err = _error_of(ref_res)

    # 6a. target rejects with 0A000 where the reference does not produce the identical error
    if tgt_err is not None and tgt_err.sqlstate == _FEATURE_NOT_SUPPORTED:
        identical = (ref_err is not None and ref_err.sqlstate == _FEATURE_NOT_SUPPORTED
                     and ref_err.message == tgt_err.message)
        if not identical:
            return Outcome.UNSUPPORTED, "first_divergence", d.kind

    # 6b. reference rejects, target accepts
    if (ref_err is not None and tgt_res is not None and tgt_err is None
            and tgt_res.kind in _SUCCESS_KINDS):
        return Outcome.UNEXPECTED_ACCEPTANCE, "first_divergence", d.kind

    # 6c. anything else
    return Outcome.MISMATCH, "first_divergence", d.kind


def _signature(case: Case, reference: Transcript, target: Transcript, d: Optional[Diff],
               outcome: Outcome, rule_kind: str) -> dict[str, str]:
    """``rule_kind`` is already the diff_kind: the rule name for rules 1-4, d.kind for rule 6,
    "" for PASS (see _decide)."""
    if d is None:
        return {
            "diff_kind": rule_kind, "sqlstate": "", "ref_sqlstate": "",
            "message_template": "", "step_kind": "", "first_keyword": "",
            "unimplemented_lease": "",
        }

    parsed = _parse_path(d.path)
    section: Optional[str] = None
    index: Optional[int] = None
    k: Optional[int] = None
    tgt_res: Optional[StatementResult] = None
    ref_res: Optional[StatementResult] = None
    if parsed is not None:
        section, index, k = parsed
        tgt_res = _result_at(_step_at(target, section, index), k)
        ref_res = _result_at(_step_at(reference, section, index), k)
    tgt_err = _error_of(tgt_res)
    ref_err = _error_of(ref_res)

    message_src = ref_err if outcome is Outcome.UNEXPECTED_ACCEPTANCE else tgt_err
    message_template = sqltext.message_template(message_src.message) if message_src else ""

    if section is None or index is None:
        step_kind = ""
        first_keyword = ""
    elif section == "setup":
        step_kind = "setup"
        first_keyword = _first_keyword_at(case, reference, target, section, index, k)
    else:
        step_kind = case.steps[index].kind if 0 <= index < len(case.steps) else ""
        first_keyword = _first_keyword_at(case, reference, target, section, index, k)

    return {
        "diff_kind": rule_kind,
        "sqlstate": tgt_err.sqlstate if tgt_err is not None else "",
        "ref_sqlstate": ref_err.sqlstate if ref_err is not None else "",
        "message_template": message_template,
        "step_kind": step_kind,
        "first_keyword": first_keyword,
        "unimplemented_lease": _unimplemented_lease(tgt_err),
    }


def _unimplemented_lease(err) -> str:
    if err is None or err.sqlstate != _FEATURE_NOT_SUPPORTED or err.detail is None:
        return ""
    m = _UNIMPLEMENTED_DETAIL_RE.fullmatch(err.detail)
    return m.group(1) if m else ""


def _first_keyword_at(case: Case, reference: Transcript, target: Transcript, section: str,
                      index: int, k: Optional[int]) -> str:
    """First keyword of the statement that produced result ``k`` (whole SQL when the SQL cannot
    be split one-statement-per-result)."""
    sql = _sql_for(case, section, index)
    if not sql:
        return ""
    if k is not None:
        statements = sqltext.split_statements(sql)
        ref_step = _step_at(reference, section, index)
        tgt_step = _step_at(target, section, index)
        if ref_step is not None:
            n_results = len(ref_step.results)
        elif tgt_step is not None:
            n_results = len(tgt_step.results)
        else:
            n_results = -1
        if len(statements) == n_results and 0 <= k < len(statements):
            sql = statements[k].text
    return sqltext.first_keyword(sql)


def _sql_for(case: Case, section: str, index: int) -> str:
    if section == "setup":
        return case.setup or ""
    if not (0 <= index < len(case.steps)):
        return ""
    return _case_step_sql(case, index)


def _case_step_sql(case: Case, index: int) -> str:
    """SQL text a case step executes. ``execute`` resolves the most recent ``parse`` of the same
    statement name on the same session; ``wait`` resolves the most recent async step on the same
    session (mirrors compare.typed.resolve_order)."""
    step = case.steps[index]
    if step.kind in ("sql", "parse", "exec_params"):
        return step.payload.get("sql") or ""
    if step.kind == "execute":
        name = step.payload.get("statement", "")
        for j in range(index - 1, -1, -1):
            prev = case.steps[j]
            if prev.kind == "parse" and prev.session == step.session \
                    and prev.payload.get("name", "") == name:
                return prev.payload.get("sql") or ""
        return ""
    if step.kind == "wait":
        for j in range(index - 1, -1, -1):
            prev = case.steps[j]
            if prev.is_async and prev.kind in ASYNC_KINDS and prev.session == step.session:
                return _case_step_sql(case, j)
        return ""
    return ""
