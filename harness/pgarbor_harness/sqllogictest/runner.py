"""ORACLE-07: run / record sqllogictest files through runner.session (simple protocol).

Lease HAR-22.

``record(input_path, endpoint)`` (oracle review path; PGARBOR_ORACLE_REVIEW=1): execute every
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

from dataclasses import dataclass, field
from pathlib import Path
from typing import Optional

from ..model import Endpoint, Outcome


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


def record(input_path: Path, endpoint: Endpoint, output_path: Path) -> list[str]:
    """Record PG results into output_path; return record-level problems ([] = success)."""
    raise NotImplementedError("HAR-22: record slt from PG")


def run(expected_path: Path, endpoint: Endpoint) -> SltFileResult:
    """Run one recorded .slt file against a target endpoint."""
    raise NotImplementedError("HAR-22: run slt file")


def run_tree(expected_root: Path, target: object, jobs: int = 4) -> list[SltFileResult]:
    """Every ``**/*.slt`` under expected_root, each in a fresh env of target
    (runner.targets.Target), sorted by path."""
    raise NotImplementedError("HAR-22: run slt tree")
