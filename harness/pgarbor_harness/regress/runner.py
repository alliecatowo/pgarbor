"""ORACLE-05: pg_regress-like schedule runner against any PG-wire target.

Lease HAR-17.

Run layout (``out_dir`` = ``runs/<campaign>/regress/<target name>/``)::

    results/<test>.out        psql output (written by regress.psql)
    regression.diffs          concatenated unified diffs of failures, schedule order
    regression.out            one line per test: ``ok 12      + boolean      23 ms`` in
                              pg_regress 18's TAP-ish format (``ok N - name  T ms`` /
                              ``not ok N - name  T ms``); exactness not required
    summary.json              list of RegressResult.to_json in schedule order

Procedure (``run_schedule``):
  1. ``prepare_database``: unless ``use_existing``: ``DROP DATABASE IF EXISTS "regression"``
     then ``CREATE DATABASE "regression" TEMPLATE=template0`` then each
     config.REGRESS_DB_SETTINGS statement (via regress.psql.run_admin_sql). If creation fails
     on the target (e.g. PGArbor without CREATE DATABASE) the WHOLE family is BLOCKED with
     reason "create_database" unless ``use_existing``.
  2. groups in schedule order; tests of a group run concurrently (ThreadPoolExecutor,
     ``max_parallel`` default 20 = pg_regress's group size bound); each test -> regress.psql.
  3. per test status (two independent verdicts):
       official:   outdiff.compare_output(results, manifest expected candidates)
                   identical -> PASS
       baseline:   outdiff.compare_output(results, [tests/expected/regress/<t>.out]) when the
                   PG oracle's recorded output exists (recorded by ``record_baseline``)
       outcome rules (model.Outcome):
         psql timed out -> HANG; psql exit 2 or output contains
           ``server closed the connection unexpectedly`` and the target is not alive ->
           CRASH; output contains ``ERROR:  could not read block`` / sqlstate XX001 text
           ``invalid page`` -> CORRUPTION;
         official identical -> PASS (family "regress-official"), and baseline PASS too;
         else baseline identical -> family "regress-official": BLOCKED(reason="oracle_env")
           if the oracle itself fails officially, else MISMATCH; family
           "regress-differential": PASS;
         else MISMATCH in both families (UNSUPPORTED instead when the first differing
           ``+`` line of the diff vs best contains ``ERROR:  `` and the feature-not-supported
           text pattern ``not supported`` / ``not implemented``).
  4. ``record_baseline`` (oracle review path only, PGARBOR_ORACLE_REVIEW=1): run the schedule
     against the PG oracle and copy each results file to ``tests/expected/regress/<t>.out``,
     plus ``tests/expected/regress/_official_status.json`` = {test: "PASS"|"FAIL"} of the
     oracle vs the official expected files (this is how "BLOCKED(oracle_env)" is decided).

A failing earlier test never stops the schedule (pg_regress semantics). If the target is no
longer alive after a CRASH, every remaining test is recorded as BLOCKED(reason="target_down")
(never PASS, never silently dropped) unless ``restart_target`` (optional zero-arg callable) is
given, in which case it is called once before the next group and the run continues.
"""

from __future__ import annotations

import json
import os
import subprocess
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Optional

from .. import config
from ..model import Outcome, write_canonical_json
from .manifest import RegressSchedule, RegressTest
from .outdiff import compare_output
from .psql import PsqlRun, run_admin_sql, run_psql

REGRESS_DB = "regression"
SETUP_TEST = "test_setup"
BASELINE_SUBDIR = "regress"
BASELINE_STATUS_FILE = "_official_status.json"
GROUP_PARALLEL = 20
CONNECTION_LOST_TEXT = "server closed the connection unexpectedly"
CORRUPTION_TEXTS = ("could not read block", "invalid page")
UNSUPPORTED_TEXTS = ("not supported", "not implemented")


@dataclass
class RegressResult:
    test: str
    group: int
    official: Outcome
    differential: Optional[Outcome]
    reason: str = ""
    diff_lines: int = 0
    elapsed_ms: int = 0
    psql_exit: Optional[int] = None
    extra: dict[str, Any] = field(default_factory=dict)

    def to_json(self) -> dict[str, Any]:
        return {"test": self.test, "group": self.group, "official": self.official.value,
                "differential": self.differential.value if self.differential else None,
                "reason": self.reason, "diff_lines": self.diff_lines,
                "elapsed_ms": self.elapsed_ms, "psql_exit": self.psql_exit,
                "extra": self.extra}


def _tail(text: str, limit: int = 500) -> str:
    text = text.strip()
    return text if len(text) <= limit else text[:limit] + "..."


def _read_text(path: Path) -> str:
    try:
        return path.read_bytes().decode("utf-8", errors="surrogateescape")
    except OSError:
        return ""


def _probe(alive: Any) -> Optional[bool]:
    """Run the zero-arg liveness probe; None when no probe was given or it failed."""
    if alive is None:
        return None
    try:
        return bool(alive())
    except Exception:  # a broken probe must not turn into a verdict
        return None


def _corrupt(text: str) -> bool:
    for line in text.splitlines():
        if line.startswith("ERROR:  ") and any(p in line for p in CORRUPTION_TEXTS):
            return True
    return False


def _unsupported(unified: str) -> bool:
    """The first differing line on the results side ('+' line) reports a missing feature."""
    for line in unified.splitlines():
        if line.startswith("+") and not line.startswith("+++"):
            body = line[1:]
            return "ERROR:  " in body and any(p in body for p in UNSUPPORTED_TEXTS)
    return False


def _ensure_newline(text: str) -> str:
    return text if not text or text.endswith("\n") else text + "\n"


def _uniform(name: str, group: int, outcome: Outcome, reason: str,
             extra: Optional[dict[str, Any]] = None) -> RegressResult:
    """Same verdict in both families (no psql run for this test)."""
    return RegressResult(test=name, group=group, official=outcome, differential=outcome,
                         reason=reason, extra=dict(extra or {}))


def _select(schedule: RegressSchedule, only: Optional[list[str]]) -> list[tuple[int, list[str]]]:
    """(original group index, selected test names) in schedule order; ``test_setup`` is always
    included when the schedule has it, so it runs first."""
    known = set(schedule.ordered_tests())
    wanted: Optional[set[str]] = None
    if only is not None:
        wanted = set(only)
        unknown = sorted(wanted - known)
        if unknown:
            raise ValueError("unknown regress test(s): " + ", ".join(unknown))
        if SETUP_TEST in known:
            wanted.add(SETUP_TEST)
    selected: list[tuple[int, list[str]]] = []
    for gi, group in enumerate(schedule.groups):
        names = [n for n in group if wanted is None or n in wanted]
        if names:
            selected.append((gi, names))
    return selected


def _load_status(baseline_dir: Optional[Path]) -> dict[str, str]:
    """Oracle official status per test ("PASS"/"FAIL"); missing or unreadable -> {} which makes
    every oracle-side failure a MISMATCH rather than BLOCKED(oracle_env)."""
    if baseline_dir is None:
        return {}
    try:
        raw = json.loads((baseline_dir / BASELINE_STATUS_FILE).read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {}
    if not isinstance(raw, dict):
        return {}
    return {str(k): str(v) for k, v in raw.items()}


def _candidates(regress_dir: Path, test: RegressTest) -> list[Path]:
    return [regress_dir / p for p in test.expected_paths]


def _judge(name: str, gi: int, test: RegressTest, regress_dir: Path, host: str, port: int,
           out_dir: Path, baseline_dir: Optional[Path], oracle_status: dict[str, str],
           target_alive: Any) -> tuple[RegressResult, str]:
    """Run one test and apply the outcome rules from the module docstring."""
    run: PsqlRun = run_psql(name, regress_dir / test.sql_path, host, port, REGRESS_DB,
                            regress_dir, out_dir)
    extra: dict[str, Any] = {}

    def done(off: Outcome, diff: Outcome, off_reason: str, diff_reason: str,
             diff_lines: int = 0, off_text: str = "", diff_text: str = "") -> tuple[RegressResult, str]:
        result = RegressResult(
            test=name, group=gi, official=off, differential=diff,
            reason=off_reason or diff_reason, diff_lines=diff_lines,
            elapsed_ms=run.elapsed_ms, psql_exit=run.exit_code,
            extra={**extra, "official_reason": off_reason, "differential_reason": diff_reason})
        parts: list[str] = []
        if off is not Outcome.PASS and off_text:
            parts.append(f"# {name}: official {off.value}\n" + _ensure_newline(off_text))
        if diff is not Outcome.PASS and diff_text:
            parts.append(f"# {name}: differential {diff.value}\n" + _ensure_newline(diff_text))
        return result, "".join(parts)

    if run.timed_out:
        return done(Outcome.HANG, Outcome.HANG, "timeout", "timeout")
    text = _read_text(run.out_file)
    if run.exit_code == 2 or CONNECTION_LOST_TEXT in text:
        alive = _probe(target_alive)
        extra["target_alive"] = alive
        if alive is False:
            return done(Outcome.CRASH, Outcome.CRASH, "crash", "crash")
    if _corrupt(text):
        return done(Outcome.CORRUPTION, Outcome.CORRUPTION, "corruption", "corruption")

    off_cmp = compare_output(run.out_file, _candidates(regress_dir, test))
    if off_cmp.identical:
        return done(Outcome.PASS, Outcome.PASS, "", "")
    off_text = off_cmp.unified
    diff_lines = max(off_cmp.diff_lines, 0)
    off_outcome = Outcome.UNSUPPORTED if _unsupported(off_text) else Outcome.MISMATCH
    off_reason = "unsupported" if off_outcome is Outcome.UNSUPPORTED else "mismatch"

    base_path = baseline_dir / f"{name}.out" if baseline_dir is not None else None
    if base_path is None or not base_path.is_file():
        return done(off_outcome, Outcome.BLOCKED, off_reason, "reference_missing",
                    diff_lines, off_text)

    base_cmp = compare_output(run.out_file, [base_path])
    if base_cmp.identical:
        if oracle_status.get(name) == "FAIL":
            return done(Outcome.BLOCKED, Outcome.PASS, "oracle_env", "", diff_lines, off_text)
        return done(Outcome.MISMATCH, Outcome.PASS, "mismatch", "", diff_lines, off_text)

    diff_outcome = Outcome.UNSUPPORTED if _unsupported(base_cmp.unified) else Outcome.MISMATCH
    diff_reason = "unsupported" if diff_outcome is Outcome.UNSUPPORTED else "mismatch"
    return done(off_outcome, diff_outcome, off_reason, diff_reason, diff_lines,
                off_text, base_cmp.unified)


def _run_one(name: str, gi: int, test: Optional[RegressTest], regress_dir: Path, host: str,
             port: int, out_dir: Path, baseline_dir: Optional[Path],
             oracle_status: dict[str, str], target_alive: Any) -> tuple[RegressResult, str]:
    if test is None:
        return _uniform(name, gi, Outcome.INVALID_CASE, "manifest_error"), ""
    try:
        return _judge(name, gi, test, regress_dir, host, port, out_dir, baseline_dir,
                      oracle_status, target_alive)
    except Exception as exc:  # recorded as a verdict, never silently dropped
        return (_uniform(name, gi, Outcome.HARNESS_ERROR,
                         f"harness_error: {type(exc).__name__}: {exc}"), "")


def _run_group(group: list[str], gi: int, schedule: RegressSchedule, regress_dir: Path,
               host: str, port: int, out_dir: Path, baseline_dir: Optional[Path],
               oracle_status: dict[str, str], target_alive: Any,
               max_parallel: int) -> dict[str, tuple[RegressResult, str]]:
    workers = max(1, min(max_parallel, len(group)))
    with ThreadPoolExecutor(max_workers=workers) as pool:
        futures = {
            n: pool.submit(_run_one, n, gi, schedule.tests.get(n), regress_dir, host, port,
                           out_dir, baseline_dir, oracle_status, target_alive)
            for n in group
        }
        return {n: f.result() for n, f in futures.items()}


def _recover(restart_target: Any, target_alive: Any) -> Optional[str]:
    """After a CRASH: None when the target is usable again, else the reason it is down."""
    if restart_target is None:
        return "no restart_target after crash"
    try:
        restart_target()
    except Exception as exc:
        return f"restart failed: {type(exc).__name__}: {exc}"
    if _probe(target_alive) is False:
        return "target still down after restart"
    return None


def _write_outputs(out_dir: Path, ordered: list[RegressResult], texts: dict[str, str]) -> None:
    lines = []
    for i, r in enumerate(ordered, 1):
        status = "ok" if r.official is Outcome.PASS else "not ok"
        lines.append(f"{status} {i} - {r.test}  {r.elapsed_ms} ms\n")
    (out_dir / "regression.out").write_bytes("".join(lines).encode("utf-8"))
    diffs = "".join(texts.get(r.test, "") for r in ordered)
    (out_dir / "regression.diffs").write_bytes(
        diffs.encode("utf-8", errors="surrogateescape"))
    write_canonical_json(out_dir / "summary.json", [r.to_json() for r in ordered])


def _record_one(name: str, test: RegressTest, regress_dir: Path, host: str, port: int,
                out_dir: Path) -> tuple[str, bool, Path]:
    """Run one test against the oracle: (official status, usable as baseline, results file)."""
    run = run_psql(name, regress_dir / test.sql_path, host, port, REGRESS_DB, regress_dir,
                   out_dir)
    if run.timed_out or not run.out_file.is_file():
        return "FAIL", False, run.out_file
    identical = compare_output(run.out_file, _candidates(regress_dir, test)).identical
    # exit 2 means the oracle connection was lost; never record that as a baseline.
    usable = run.exit_code in (0, 1)
    return ("PASS" if identical else "FAIL"), usable, run.out_file


def prepare_database(host: str, port: int, use_existing: bool, db: str = "regression") -> \
        Optional[str]:
    """Step 1; return None on success or an error string (family BLOCKED)."""
    if use_existing:
        return None
    steps = [f'DROP DATABASE IF EXISTS "{db}"', f'CREATE DATABASE "{db}" TEMPLATE=template0']
    steps += [s.format(db=db) for s in config.REGRESS_DB_SETTINGS]
    for sql in steps:
        try:
            code, out = run_admin_sql(sql, host, port)
        except (OSError, subprocess.SubprocessError) as exc:
            return f"{sql}: {exc}"
        if code != 0:
            return f"{sql}: exit {code}: {_tail(out)}"
    return None


def run_schedule(schedule: RegressSchedule, regress_dir: Path, host: str, port: int,
                 out_dir: Path, baseline_dir: Optional[Path], target_alive: Any = None,
                 use_existing: bool = False, max_parallel: int = 20,
                 only: Optional[list[str]] = None,
                 restart_target: Any = None) -> list[RegressResult]:
    """Steps 1-3. ``target_alive`` / ``restart_target`` are optional zero-arg callables.
    ``only`` restricts to listed tests but ALWAYS includes ``test_setup`` first (other tests
    depend on it), preserving schedule order."""
    if max_parallel < 1:
        raise ValueError("max_parallel must be at least 1")
    selected = _select(schedule, only)
    names_in_order = [n for _, group in selected for n in group]
    out_dir.mkdir(parents=True, exist_ok=True)
    by_name: dict[str, RegressResult] = {}
    texts: dict[str, str] = {}

    error = prepare_database(host, port, use_existing)
    if error is not None:
        for gi, group in selected:
            for n in group:
                by_name[n] = _uniform(n, gi, Outcome.BLOCKED, "create_database",
                                      {"error": error})
    else:
        oracle_status = _load_status(baseline_dir)
        down_detail: Optional[str] = None
        for pos, (gi, group) in enumerate(selected):
            if down_detail is not None:
                for n in group:
                    by_name[n] = _uniform(n, gi, Outcome.BLOCKED, "target_down",
                                          {"detail": down_detail})
                continue
            outcomes = _run_group(group, gi, schedule, regress_dir, host, port, out_dir,
                                  baseline_dir, oracle_status, target_alive, max_parallel)
            crashed = False
            for n in group:
                result, text = outcomes[n]
                by_name[n] = result
                texts[n] = text
                crashed = crashed or result.official is Outcome.CRASH
            if crashed and pos + 1 < len(selected):
                down_detail = _recover(restart_target, target_alive)

    ordered = [by_name[n] for n in names_in_order]
    _write_outputs(out_dir, ordered, texts)
    return ordered


def record_baseline(schedule: RegressSchedule, regress_dir: Path, oracle_host: str,
                    oracle_port: int, out_dir: Path, expected_root: Path) -> dict[str, str]:
    """Step 4. Returns {test: official status on the oracle}."""
    if os.environ.get("PGARBOR_ORACLE_REVIEW") != "1":
        raise RuntimeError("record_baseline is the oracle review path; "
                           "set PGARBOR_ORACLE_REVIEW=1 to record baselines")
    error = prepare_database(oracle_host, oracle_port, False)
    if error is not None:
        raise RuntimeError(f"cannot prepare the oracle regression database: {error}")
    out_dir.mkdir(parents=True, exist_ok=True)
    baseline_dir = expected_root / BASELINE_SUBDIR
    baseline_dir.mkdir(parents=True, exist_ok=True)
    status: dict[str, str] = {}
    for group in schedule.groups:
        items = [n for n in group if n in schedule.tests]
        if not items:
            continue
        workers = max(1, min(GROUP_PARALLEL, len(items)))
        with ThreadPoolExecutor(max_workers=workers) as pool:
            futures = {
                n: pool.submit(_record_one, n, schedule.tests[n], regress_dir, oracle_host,
                               oracle_port, out_dir)
                for n in items
            }
            recorded = {n: f.result() for n, f in futures.items()}
        for n in items:
            st, usable, out_file = recorded[n]
            status[n] = st
            dst = baseline_dir / f"{n}.out"
            if usable and out_file.is_file():
                tmp = dst.with_name(dst.name + ".tmp")
                tmp.write_bytes(out_file.read_bytes())
                os.replace(tmp, dst)
            else:
                dst.unlink(missing_ok=True)  # never keep a stale or partial baseline
    write_canonical_json(baseline_dir / BASELINE_STATUS_FILE, status)
    return status
