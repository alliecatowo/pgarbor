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

from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Optional

from ..model import Outcome
from .manifest import RegressSchedule


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


def prepare_database(host: str, port: int, use_existing: bool, db: str = "regression") -> \
        Optional[str]:
    """Step 1; return None on success or an error string (family BLOCKED)."""
    raise NotImplementedError("HAR-17: prepare regression database")


def run_schedule(schedule: RegressSchedule, regress_dir: Path, host: str, port: int,
                 out_dir: Path, baseline_dir: Optional[Path], target_alive: Any = None,
                 use_existing: bool = False, max_parallel: int = 20,
                 only: Optional[list[str]] = None,
                 restart_target: Any = None) -> list[RegressResult]:
    """Steps 1-3. ``target_alive`` / ``restart_target`` are optional zero-arg callables.
    ``only`` restricts to listed tests but ALWAYS includes ``test_setup`` first (other tests
    depend on it), preserving schedule order."""
    raise NotImplementedError("HAR-17: run regress schedule")


def record_baseline(schedule: RegressSchedule, regress_dir: Path, oracle_host: str,
                    oracle_port: int, out_dir: Path, expected_root: Path) -> dict[str, str]:
    """Step 4. Returns {test: official status on the oracle}."""
    raise NotImplementedError("HAR-17: record PG baseline outputs")
