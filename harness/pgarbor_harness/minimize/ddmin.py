"""ORACLE-10: discrepancy minimizer (delta debugging over statements and steps).

Lease HAR-26.

Algorithm: Zeller & Hildebrandt ddmin (complement-first variant) over a list of units,
with an ``interesting(units) -> bool`` oracle; returns a 1-minimal list (removing any single
unit makes it uninteresting) — guaranteed by a final single-unit-removal pass.
Determinism: partitions are contiguous chunks in original order; no randomness; results are
memoized by the tuple of unit indices (``cache`` dict) so each candidate is executed at most
once.

Units for a Case (``minimize_case``):
  1. the case's ``setup`` split into statements (sqltext.split_statements) — each statement a
     unit (rejoined with ";\\n");
  2. the case's steps — each step a unit, EXCEPT that structural pairs are kept together:
     an async step and its ``wait``; a ``parse`` and every later step naming its statement;
     removing one member removes the group (so candidates always validate);
  3. then, inside each remaining ``sql`` step with several statements, its statements.
Phases run in that order, each to fixpoint.

Interesting predicate for a case (``same_failure``): build the candidate Case dict, validate
it (invalid -> not interesting), run it on the REFERENCE (oracle, fresh db) and on the TARGET,
compare + classify; interesting iff the outcome is identical to the original AND the
signature fields ``diff_kind``, ``sqlstate``, ``ref_sqlstate``, ``message_template`` are
identical. A reference run that errors where the original reference did not is fine (the
signature captures what matters) but a reference HARNESS_ERROR/timeout is not interesting.

Plain SQL scripts (``minimize_script``, e.g. a failing regress file): units = statements;
the caller supplies ``interesting``.

Budget: ``max_runs`` (default 400 candidate executions); when exhausted return the best found
with ``complete=False``.

Output: ``MinimizeResult.case_dict`` written by the CLI to
``runs/<campaign>/minimized/<case_id>.min.yaml`` with ``provenance: {source: minimized,
ref: <original case id>}`` and id ``<family>.min-<original name>``; promotion into
tests/cases is a separate (research / oracle review) step.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Callable, Sequence, TypeVar

T = TypeVar("T")


@dataclass
class MinimizeResult:
    units_before: int
    units_after: int
    runs: int
    complete: bool
    case_dict: dict[str, Any] = field(default_factory=dict)
    script: str = ""


def ddmin(units: Sequence[T], interesting: Callable[[list[T]], bool],
          max_runs: int = 400) -> tuple[list[T], int, bool]:
    """Generic ddmin; returns (minimal units, runs used, complete). Precondition:
    ``interesting(list(units))`` is True (checked; if False raise ValueError("original input
    is not interesting"))."""
    raise NotImplementedError("HAR-26: ddmin core")


def minimize_script(script: str, interesting: Callable[[str], bool],
                    max_runs: int = 400) -> MinimizeResult:
    """Statement-level minimization of a SQL script."""
    raise NotImplementedError("HAR-26: minimize sql script")


def minimize_case(case_dict: dict[str, Any], same_failure: Callable[[dict[str, Any]], bool],
                  max_runs: int = 400) -> MinimizeResult:
    """Phased minimization of a case dict (module docstring)."""
    raise NotImplementedError("HAR-26: minimize case")


def same_failure_predicate(original: Any, reference_target: Any, target: Any) -> \
        Callable[[dict[str, Any]], bool]:
    """Build the case predicate: ``original`` is the model.CaseResult of the failing case;
    ``reference_target`` / ``target`` are runner.targets.Target objects."""
    raise NotImplementedError("HAR-26: failure-preserving predicate")
