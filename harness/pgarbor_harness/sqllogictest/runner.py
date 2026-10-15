"""ORACLE-07: run / record sqllogictest files through runner.session (simple protocol).

Lease HAR-22.

``record(input_path, endpoint, output_path)`` (oracle review path; PGARBOR_ORACLE_REVIEW=1): execute every
record against the PG oracle in one session of a fresh database; fill ``expected`` of
``query`` records (render_values with the record's sort mode and the current hash-threshold)
and turn ``statement`` records into ``statement ok`` or ``statement error <sqlstate>``
according to what PG did (an authored ``statement ok`` that errors on PG is an
INVALID_CASE-style record error; recording fails for that file and lists the record lines).
Output written to ``tests/expected/slt/<same relative path>``.

``run(expected_path, endpoint)``: execute against a target; per record verdict:
  * statement ok: target must not error; statement error <S>: target must error with S;
  * query: target column count must equal len(types); rendered values must equal expected;
  * ``skipif pgarbor`` / ``onlyif <other>`` records are skipped (reported, not passed);
  * ``halt`` stops the file.
File outcome (model.Outcome): PASS if every executed record matched and none skipped; first
failing record decides: target error 0A000 where PG succeeded -> UNSUPPORTED; PG errored but
target succeeded -> UNEXPECTED_ACCEPTANCE; connection lost -> CRASH; timeout -> HANG; else
MISMATCH. Skipped records make an otherwise-passing file BLOCKED(reason="skipped_records").
Sqllogictest checks query results only — never durability or concurrency (RFC 003 §6.5).
"""

from __future__ import annotations

import os
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, field
from pathlib import Path
from typing import Optional, Sequence

from .. import config
from ..model import ErrorInfo, Endpoint, Outcome, StatementResult
from ..runner.session import Session, StepOutput
from ..runner.targets import TargetStartError
from .parser import SltRecord, SltSyntaxError, parse, render_values, write


@dataclass
class SltFileResult:
    path: str
    outcome: Outcome
    reason: str = ""
    records_total: int = 0
    records_passed: int = 0
    records_skipped: int = 0
    first_failure_line: Optional[int] = None
    details: list[str] = field(default_factory=list)


#: Engine names that identify the PG reference (conditions ``skipif``/``onlyif`` while recording).
PG_ENGINES: frozenset[str] = frozenset({"postgresql", "postgres", "pg", "pg18"})
#: Engine name of PGArbor (conditions while running).
PGARBOR_ENGINES: frozenset[str] = frozenset({"pgarbor"})

#: PostgreSQL error class 0A000 (feature_not_supported): a target-side "not implemented yet".
UNSUPPORTED_SQLSTATE = "0A000"

_RECORD_KINDS = ("statement", "query")


# ---------------------------------------------------------------------------------------------
# shared helpers
# ---------------------------------------------------------------------------------------------


def _excluded(rec: SltRecord, engines: frozenset[str]) -> bool:
    """True when the record's conditions exclude the engine set (``skipif <e>`` with e in the
    set, or ``onlyif <e>`` with e outside it)."""
    for kind, engine in rec.conditions:
        name = engine.strip().lower()
        if kind == "skipif" and name in engines:
            return True
        if kind == "onlyif" and name not in engines:
            return True
    return False


def _first_error(results: Sequence[StatementResult]) -> Optional[ErrorInfo]:
    """The first protocol-level error result, or None when every result succeeded."""
    for r in results:
        if r.kind == "error":
            if r.error is not None:
                return r.error
            # kind "error" without an ErrorResponse cannot come from a valid session; keep it an
            # error rather than silently passing it.
            return ErrorInfo(severity="ERROR", sqlstate="XX000",
                             message="error result without ErrorResponse fields")
    return None


def _as_text(v: object) -> Optional[str]:
    if v is None or isinstance(v, str):
        return v
    if isinstance(v, (bytes, bytearray)):
        return bytes(v).decode("utf-8", errors="backslashreplace")
    return str(v)


def _describe_diff(expected: list[str], actual: list[str]) -> str:
    parts = []
    if len(expected) != len(actual):
        parts.append(f"expected {len(expected)} values, got {len(actual)}")
    for i, (e, a) in enumerate(zip(expected, actual)):
        if e != a:
            parts.append(f"value {i}: expected {e!r}, got {a!r}")
            break
    return "; ".join(parts) if parts else "values differ"


def _finalize(result: SltFileResult) -> SltFileResult:
    """A passing file with skipped records is BLOCKED, never PASS (acceptance)."""
    if result.outcome is Outcome.PASS and result.records_skipped > 0:
        result.outcome = Outcome.BLOCKED
        result.reason = "skipped_records"
    return result


# ---------------------------------------------------------------------------------------------
# record path (PG oracle)
# ---------------------------------------------------------------------------------------------


def _record_statement(rec: SltRecord, err: Optional[ErrorInfo], problems: list[str]) -> None:
    if err is None:
        if not rec.expect_ok:
            problems.append(f"line {rec.line}: 'statement error' but PG accepted the statement")
        rec.expect_ok = True
        rec.sqlstate = None
        return
    if rec.expect_ok:
        problems.append(f"line {rec.line}: 'statement ok' but PG returned {err.sqlstate}: "
                        f"{err.message}")
    elif rec.sqlstate is not None and rec.sqlstate != err.sqlstate:
        problems.append(f"line {rec.line}: authored SQLSTATE {rec.sqlstate} but PG returned "
                        f"{err.sqlstate}: {err.message}")
    rec.expect_ok = False
    rec.sqlstate = err.sqlstate


def _record_query(rec: SltRecord, out: StepOutput, threshold: int,
                  problems: list[str]) -> None:
    err = _first_error(out.results)
    if err is not None:
        problems.append(f"line {rec.line}: query failed on PG: {err.sqlstate}: {err.message}")
        return
    if len(out.results) != 1 or out.results[0].kind != "rows":
        kinds = ", ".join(r.kind for r in out.results) or "none"
        problems.append(f"line {rec.line}: query must produce exactly one row set (got {kinds})")
        return
    res = out.results[0]
    if len(res.columns) != len(rec.types):
        problems.append(f"line {rec.line}: query has {len(res.columns)} columns on PG but "
                        f"types {rec.types!r} declare {len(rec.types)}")
        return
    rows: list[list[Optional[str]]] = []
    for row in res.rows:
        cells: list[Optional[str]] = []
        for v in row:
            if isinstance(v, (bytes, bytearray)):
                problems.append(f"line {rec.line}: value is not UTF-8 text; cannot record")
                return
            cells.append(v)
        rows.append(cells)
    rec.expected = render_values(rows, rec.sort, threshold)


def _record_records(records: Sequence[SltRecord], session: object,
                    problems: list[str]) -> None:
    """Execute the records on ``session`` in order, filling expectations in place. Stops at
    ``halt``, at the first timeout or lost connection (each appends one problem)."""
    threshold = 0
    for rec in records:
        if rec.kind == "hash-threshold":
            threshold = rec.hash_threshold
            continue
        if rec.kind == "halt":
            break
        if rec.kind not in _RECORD_KINDS:
            continue
        if _excluded(rec, PG_ENGINES):
            if not _excluded(rec, PGARBOR_ENGINES):
                problems.append(f"line {rec.line}: record is excluded from the PG recording "
                                "but runs on pgarbor; cannot record it")
            continue  # kept verbatim: the runner skips it for pgarbor too
        out: StepOutput = session.run_simple(rec.sql)  # type: ignore[attr-defined]
        if out.timed_out:
            problems.append(f"line {rec.line}: PG oracle timed out")
            return
        if out.connection_lost:
            problems.append(f"line {rec.line}: PG oracle connection lost")
            return
        if rec.kind == "statement":
            _record_statement(rec, _first_error(out.results), problems)
        else:
            _record_query(rec, out, threshold, problems)


def record(input_path: Path, endpoint: Endpoint, output_path: Path) -> list[str]:
    """Record PG results into output_path; return record-level problems ([] = success).

    Refuses (PermissionError) unless PGARBOR_ORACLE_REVIEW=1. ``endpoint`` must be a fresh
    database provisioned by the caller. Nothing is written when any problem is found.
    """
    if os.environ.get("PGARBOR_ORACLE_REVIEW") != "1":
        raise PermissionError("slt record can only run through the oracle review path "
                              "(PGARBOR_ORACLE_REVIEW=1)")
    name = str(input_path)
    try:
        text = Path(input_path).read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError) as e:
        return [f"{name}: cannot read: {e}"]
    try:
        records = parse(text, name)
    except SltSyntaxError as e:
        return [str(e)]

    session = Session(endpoint, "slt-record", step_timeout_s=config.STEP_TIMEOUT_S)
    try:
        session.connect()
    except ConnectionError as e:
        return [f"{name}: cannot connect to PG oracle: {e}"]
    problems: list[str] = []
    try:
        _record_records(records, session, problems)
    finally:
        session.close()
    if problems:
        return problems
    out_path = Path(output_path)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(write(records), encoding="utf-8")
    return []


# ---------------------------------------------------------------------------------------------
# run path (target)
# ---------------------------------------------------------------------------------------------


def _judge(rec: SltRecord, out: StepOutput, threshold: int) -> tuple[Optional[Outcome], str]:
    """Verdict for one executed record: (None, "") when it matched, else (outcome, reason)."""
    if out.timed_out:
        return Outcome.HANG, f"line {rec.line}: step timed out"
    if out.connection_lost:
        return Outcome.CRASH, f"line {rec.line}: connection lost"
    err = _first_error(out.results)

    if rec.kind == "statement":
        if rec.expect_ok:
            if err is None:
                return None, ""
            if err.sqlstate == UNSUPPORTED_SQLSTATE:
                return Outcome.UNSUPPORTED, (f"line {rec.line}: statement ok: target "
                                             f"{err.sqlstate} ({err.message}); PG succeeded")
            return Outcome.MISMATCH, (f"line {rec.line}: statement ok: target error "
                                      f"{err.sqlstate}: {err.message}")
        if err is None:
            return Outcome.UNEXPECTED_ACCEPTANCE, (f"line {rec.line}: statement error "
                                                   f"{rec.sqlstate}: target accepted it")
        if rec.sqlstate is None or err.sqlstate == rec.sqlstate:
            return None, ""
        return Outcome.MISMATCH, (f"line {rec.line}: statement error {rec.sqlstate}: target "
                                  f"returned {err.sqlstate}: {err.message}")

    # query
    if err is not None:
        if err.sqlstate == UNSUPPORTED_SQLSTATE:
            return Outcome.UNSUPPORTED, (f"line {rec.line}: query: target {err.sqlstate} "
                                         f"({err.message}); PG succeeded")
        return Outcome.MISMATCH, f"line {rec.line}: query: target error {err.sqlstate}: {err.message}"
    if len(out.results) != 1 or out.results[0].kind != "rows":
        kinds = ", ".join(r.kind for r in out.results) or "none"
        return Outcome.MISMATCH, f"line {rec.line}: query: target did not return one row set ({kinds})"
    res = out.results[0]
    if len(res.columns) != len(rec.types):
        return Outcome.MISMATCH, (f"line {rec.line}: query: target has {len(res.columns)} "
                                  f"columns, types {rec.types!r} declare {len(rec.types)}")
    rows = [[_as_text(v) for v in row] for row in res.rows]
    actual = render_values(rows, rec.sort, threshold)
    expected = rec.expected or []
    if actual != expected:
        return Outcome.MISMATCH, f"line {rec.line}: query: {_describe_diff(expected, actual)}"
    return None, ""


def _run_records(records: Sequence[SltRecord], session: object,
                 result: SltFileResult) -> None:
    """Execute the records on ``session``; update ``result`` in place. The first failing
    record decides the outcome and stops the file. Skipped records are counted, not run."""
    threshold = 0
    for rec in records:
        if rec.kind == "hash-threshold":
            threshold = rec.hash_threshold
            continue
        if rec.kind == "halt":
            break
        if rec.kind not in _RECORD_KINDS:
            continue
        if _excluded(rec, PGARBOR_ENGINES):
            result.records_skipped += 1
            result.details.append(f"line {rec.line}: skipped by condition")
            continue
        if rec.kind == "query" and rec.expected is None:
            result.outcome = Outcome.INVALID_CASE
            result.reason = f"line {rec.line}: query has no recorded results"
            result.first_failure_line = rec.line
            return
        out: StepOutput = session.run_simple(rec.sql)  # type: ignore[attr-defined]
        outcome, message = _judge(rec, out, threshold)
        if outcome is None:
            result.records_passed += 1
            continue
        result.outcome = outcome
        result.reason = message
        result.first_failure_line = rec.line
        result.details.append(message)
        return


def run(expected_path: Path, endpoint: Endpoint) -> SltFileResult:
    """Run one recorded .slt file against a target endpoint."""
    path_str = str(expected_path)
    try:
        text = Path(expected_path).read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError) as e:
        return SltFileResult(path_str, Outcome.HARNESS_ERROR, reason=f"read: {e}")
    try:
        records = parse(text, path_str)
    except SltSyntaxError as e:
        return SltFileResult(path_str, Outcome.INVALID_CASE, reason=f"syntax: {e}")

    result = SltFileResult(
        path_str, Outcome.PASS,
        records_total=sum(1 for r in records if r.kind in _RECORD_KINDS),
    )
    session = Session(endpoint, "slt-run", step_timeout_s=config.STEP_TIMEOUT_S)
    try:
        session.connect()
    except ConnectionError as e:
        result.outcome = Outcome.CRASH
        result.reason = f"connect failed: {e}"
        return result
    except Exception as e:  # harness-side failure, never a verdict on the target
        result.outcome = Outcome.HARNESS_ERROR
        result.reason = f"exception:{type(e).__name__}: {e}"
        return result
    try:
        _run_records(records, session, result)
    except Exception as e:  # harness-side failure (e.g. HarnessSessionError)
        result.outcome = Outcome.HARNESS_ERROR
        result.reason = f"exception:{type(e).__name__}: {e}"
    finally:
        session.close()
    return _finalize(result)


def _run_tree_one(root: Path, file: Path, target: object) -> SltFileResult:
    rel = file.relative_to(root).as_posix()
    case_id = "slt/" + rel.removesuffix(".slt")
    try:
        env = target.provision(case_id, 1)  # type: ignore[attr-defined]
    except TargetStartError as e:
        return SltFileResult(rel, Outcome.CRASH, reason=f"target_start: {e}")
    except Exception as e:
        return SltFileResult(rel, Outcome.HARNESS_ERROR,
                             reason=f"provision exception:{type(e).__name__}: {e}")
    try:
        result = run(file, env.endpoint)
    except Exception as e:
        result = SltFileResult(rel, Outcome.HARNESS_ERROR,
                               reason=f"exception:{type(e).__name__}: {e}")
    try:
        target.release(env)  # type: ignore[attr-defined]
    except Exception as e:
        result.details.append(f"release failed: {type(e).__name__}: {e}")
    result.path = rel
    return result


def run_tree(expected_root: Path, target: object, jobs: int = 4) -> list[SltFileResult]:
    """Every ``**/*.slt`` under expected_root, each in a fresh env of target
    (runner.targets.Target), sorted by path.

    Calls ``target.start()`` once before and ``target.stop()`` once after (same as
    ``runner.differential.run_cases``); each file gets its own ``provision``/``release``.
    """
    root = Path(expected_root)
    files = sorted(root.rglob("*.slt"))
    if not files:
        return []
    target.start()  # type: ignore[attr-defined]
    try:
        if jobs <= 1 or len(files) == 1:
            results = [_run_tree_one(root, f, target) for f in files]
        else:
            with ThreadPoolExecutor(max_workers=jobs) as pool:
                results = list(pool.map(lambda f: _run_tree_one(root, f, target), files))
    finally:
        target.stop()  # type: ignore[attr-defined]
    results.sort(key=lambda r: r.path)
    return results
