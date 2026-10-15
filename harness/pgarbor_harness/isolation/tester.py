"""ORACLE-06: isolationtester-compatible runner (output diffable against expected/*.out).

Lease HAR-20 (sonnet: concurrent sessions, wait detection, exact output formatting).

Re-implements src/test/isolation/isolationtester.c behavior (reference reading) in Python on
top of runner.session.Session (async start/poll/finish):

Per spec: print ``Parsed test spec with N sessions\\n``; per permutation:
``\\nstarting permutation: s1 s2 ...\\n``; run main setup blocks on the control connection,
per-session setups, then steps, then per-session teardowns, then main teardown (setup/teardown
result sets are printed with print_result_set; errors in setup abort the test: ``setup failed:
<msg>`` -> HARNESS-level failure of this spec).

Step output lines (exact):
  * ``step <name>: <sql>`` followed by `` <waiting ...>`` when the step is detected waiting,
    newline;
  * on later completion of a waiting step: ``step <name>: <... completed>\\n``;
  * result sets via ``print_result_set`` (PQprint with header, align, fieldSep "|"): see
    example below — column header centered/right/left exactly as PQprint does (libpq
    fe-print.c; numeric-looking columns are right aligned), separator line of '-' with '+'
    between columns, then rows, then ``(N row)``/``(N rows)`` and a blank line;
  * errors (isolationtester.c ~line 1023): ``f"{severity}:  {message_primary}\\n"`` — ONLY
    the severity and primary message (no DETAIL/HINT/CONTEXT); only when either field is
    missing, the whole PQresultErrorMessage text + newline;
  * NOTIFY messages received are printed after the step's results as
    ``NOTIFY "<channel>" with payload "<payload>" from <session name or pid>``;
  * notices: ``<session>: NOTICE:  <message>\\n`` printed when received.

Example (expected/read-write-unique.out)::

    step r2: SELECT * FROM test;
     i
    --
    42
    (1 row)

    step w2: INSERT INTO test VALUES (42); <waiting ...>
    step c1: COMMIT;
    step w2: <... completed>
    ERROR:  could not serialize access due to read/write dependencies among transactions

Wait detection (isolationtester.c line ~222): after launching a step, if not complete, ask the
control connection ``SELECT pg_catalog.pg_isolation_test_session_is_blocked($1, '{<all other
session pids>}')`` with the step session's backend pid (``SELECT pg_backend_pid()`` at
session start). True -> waiting. Markers (``*``, ``<step>``, ``<step> notices N``) delay the
reported completion as described in the isolation README.

Targets without ``pg_isolation_test_session_is_blocked`` (detected once: the control query
fails with 42883 undefined_function): fall back to HEURISTIC detection — a step still running
after ``config.BLOCK_SETTLE_S`` is considered waiting. Under heuristic detection the spec's
Outcome is NEVER PASS: identical output -> BLOCKED(reason="heuristic_wait_detection");
output differing only in lines containing ``<waiting ...>`` or ``<... completed>`` -> FLAKY;
other differences -> MISMATCH.

Per-spec outcome with real detection: identical to any expected variant (regress.outdiff
rules) -> PASS; else MISMATCH (CRASH/HANG as in regress.runner). Step timeout: 2 x
config.STEP_TIMEOUT_S like isolationtester's 2 * PG_TEST_TIMEOUT_DEFAULT -> cancel -> HANG.

Each spec runs in a fresh database (target.provision) — isolationtester itself runs all
specs in one ``isolation_regression`` database; fresh databases are stricter and equivalent
because every spec's teardown drops its objects.
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Optional

from ..model import Endpoint, Outcome, StatementResult
from .spec_parser import IsolationSpec


@dataclass
class IsolationResult:
    test: str
    outcome: Outcome
    reason: str
    output_path: Optional[str]
    diff_lines: int = 0
    wait_detection: str = "function"   #: "function" | "heuristic"
    elapsed_ms: int = 0


def print_result_set(res: StatementResult) -> str:
    """Render a rows result exactly like libpq PQprint(header=1, align=1, fieldSep="|")."""
    raise NotImplementedError("HAR-20: PQprint-compatible rendering")


def run_spec(spec: IsolationSpec, endpoint: Endpoint) -> tuple[str, str]:
    """Run all permutations; return (output text, wait_detection mode)."""
    raise NotImplementedError("HAR-20: run isolation spec")


def run_suite(isolation_dir: Path, target: object, out_dir: Path,
              only: Optional[list[str]] = None, jobs: int = 4) -> list[IsolationResult]:
    """Parse the schedule, run each spec in a fresh env of ``target`` (runner.targets.Target),
    write ``out_dir/results/<test>.out``, compare with expected variants (regress.outdiff),
    classify as in the module docstring. Unparseable spec -> HARNESS_ERROR."""
    raise NotImplementedError("HAR-20: run isolation suite")
