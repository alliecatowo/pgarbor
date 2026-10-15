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

import copy
import json
from dataclasses import dataclass, field
from typing import Any, Callable, Optional, Sequence, TypeVar

from ..cases.validate import validate
from ..sqltext import split_statements

T = TypeVar("T")

_STEP_KINDS: tuple[str, ...] = ("sql", "parse", "describe", "execute", "exec_params", "close",
                                "wait", "reconnect", "restart")
_SIGNATURE_FIELDS: tuple[str, ...] = ("diff_kind", "sqlstate", "ref_sqlstate",
                                      "message_template")


@dataclass
class MinimizeResult:
    units_before: int
    units_after: int
    runs: int
    complete: bool
    case_dict: dict[str, Any] = field(default_factory=dict)
    script: str = ""


class NotInterestingError(ValueError):
    """The original input given to ddmin does not satisfy the oracle."""


class _Exhausted(Exception):
    """Internal: the run budget is spent; the best result so far is returned."""


# ---------------------------------------------------------------------------------------------
# Generic ddmin
# ---------------------------------------------------------------------------------------------


def _partition(seq: list[int], n: int) -> list[list[int]]:
    """Split ``seq`` into ``n`` contiguous, non-empty chunks (requires 1 <= n <= len(seq))."""
    length = len(seq)
    return [seq[i * length // n:(i + 1) * length // n] for i in range(n)]


def ddmin(units: Sequence[T], interesting: Callable[[list[T]], bool],
          max_runs: int = 400) -> tuple[list[T], int, bool]:
    """Generic ddmin; returns (minimal units, runs used, complete). Precondition:
    ``interesting(list(units))`` is True (checked; if False raise ValueError("original input
    is not interesting"))."""
    if max_runs < 1:
        raise ValueError("max_runs must be at least 1")
    items = list(units)
    cache: dict[tuple[int, ...], bool] = {}
    runs = 0

    def test(idx: list[int]) -> bool:
        nonlocal runs
        key = tuple(idx)
        hit = cache.get(key)
        if hit is not None:
            return hit
        if runs >= max_runs:
            raise _Exhausted()
        runs += 1
        verdict = bool(interesting([items[i] for i in key]))
        cache[key] = verdict
        return verdict

    current: list[int] = list(range(len(items)))
    if not test(current):
        raise NotInterestingError("original input is not interesting")

    try:
        n = 2
        while len(current) >= 2:
            n = min(n, len(current))
            chunks = _partition(current, n)
            progressed = False
            # complement-first: try removing one chunk at a time
            for i in range(n):
                complement = [x for j in range(n) if j != i for x in chunks[j]]
                if test(complement):
                    current = complement
                    n = max(n - 1, 2)
                    progressed = True
                    break
            # reduce to a subset (only distinct from the complement test when n > 2)
            if not progressed and n > 2:
                for chunk in chunks:
                    if test(chunk):
                        current = chunk
                        n = 2
                        progressed = True
                        break
            if not progressed:
                if n >= len(current):
                    break
                n = min(2 * n, len(current))

        # 1-minimality: no single unit can be removed while staying interesting
        changed = True
        while changed:
            changed = False
            for i in range(len(current)):
                candidate = current[:i] + current[i + 1:]
                if test(candidate):
                    current = candidate
                    changed = True
                    break
    except _Exhausted:
        return [items[i] for i in current], runs, False
    return [items[i] for i in current], runs, True


# ---------------------------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------------------------


def _texts(sql: str) -> list[str]:
    return [s.text for s in split_statements(sql)]


def _join(statements: Sequence[str]) -> str:
    """Rejoin statements as ``stmt;\\n stmt;`` . A statement whose last line ends in a line
    comment gets a newline first so the ';' is not swallowed by the comment."""
    parts: list[str] = []
    for text in statements:
        last_line = text.rsplit("\n", 1)[-1]
        parts.append(text + ("\n" if "--" in last_line else ""))
    return ";\n".join(parts) + ";"


def _rel_path(case: dict[str, Any]) -> str:
    family = str(case.get("family", ""))
    cid = str(case.get("id", ""))
    name = cid.split(".", 1)[1] if "." in cid else cid
    return f"tests/cases/{family}/{name}.yaml"


def _step_groups(case: dict[str, Any]) -> list[list[int]]:
    """Partition the step indices of a case dict into atomic groups (ascending order)."""
    steps = case.get("steps") or []
    sessions = case.get("sessions") or ["a"]
    default_session = sessions[0]
    n = len(steps)

    kinds: list[str] = []
    sess: list[str] = []
    raws: list[Any] = []
    for step in steps:
        kind = next((k for k in _STEP_KINDS if isinstance(step, dict) and k in step), "")
        kinds.append(kind)
        raws.append(step.get(kind) if kind else None)
        sess.append("" if kind == "restart" else step.get("session", default_session))

    parent = list(range(n))

    def find(x: int) -> int:
        while parent[x] != x:
            parent[x] = parent[parent[x]]
            x = parent[x]
        return x

    def union(a: int, b: int) -> None:
        ra, rb = find(a), find(b)
        if ra != rb:
            parent[max(ra, rb)] = min(ra, rb)

    for i in range(n):
        kind = kinds[i]
        if kind in ("sql", "execute", "exec_params") and steps[i].get("async") is True:
            # the matching wait is the first later wait on the same session
            for j in range(i + 1, n):
                if kinds[j] == "wait" and sess[j] == sess[i]:
                    union(i, j)
                    break
        elif kind == "parse":
            name = raws[i].get("name", "") if isinstance(raws[i], dict) else ""
            for j in range(i + 1, n):
                if kinds[j] == "restart":
                    break  # restart forgets every prepared statement
                if sess[j] != sess[i]:
                    continue
                if kinds[j] == "reconnect":
                    break
                if kinds[j] == "parse":
                    other = raws[j].get("name", "") if isinstance(raws[j], dict) else ""
                    if other == name:
                        break
                    continue
                if kinds[j] in ("describe", "execute", "close") and isinstance(raws[j], dict):
                    if raws[j].get("statement", "") == name:
                        union(i, j)

    groups: dict[int, list[int]] = {}
    for i in range(n):
        groups.setdefault(find(i), []).append(i)
    return sorted(groups.values(), key=lambda g: g[0])


def _count_units(case: dict[str, Any]) -> int:
    total = 0
    setup = case.get("setup")
    if isinstance(setup, str):
        total += len(_texts(setup))
    total += len(_step_groups(case))
    for step in case.get("steps") or []:
        if isinstance(step, dict) and isinstance(step.get("sql"), str):
            total += len(_texts(step["sql"]))
    return total


class _Minimizer:
    """State for the phased case minimization (see module docstring)."""

    def __init__(self, case: dict[str, Any], same_failure: Callable[[dict[str, Any]], bool],
                 max_runs: int):
        self.cur: dict[str, Any] = copy.deepcopy(case)
        self.same_failure = same_failure
        self.max_runs = max_runs
        self.runs = 0
        self.complete = True
        self._memo: dict[str, bool] = {}

    def ok(self, candidate: dict[str, Any]) -> bool:
        """Validate, then execute ``same_failure``; each distinct candidate runs once."""
        key = json.dumps(candidate, sort_keys=True, default=str)
        hit = self._memo.get(key)
        if hit is None:
            hit = (not validate(candidate, _rel_path(candidate))
                   and bool(self.same_failure(candidate)))
            self._memo[key] = hit
        return hit

    def reduce(self, units: list[Any],
               build: Callable[[dict[str, Any], list[Any]], Optional[dict[str, Any]]]
               ) -> Optional[list[Any]]:
        """ddmin over ``units`` for the current case; ``build(base, sub)`` makes a candidate
        (None = not a candidate). Returns the kept units, or None when the phase could not
        run (budget spent or original not interesting)."""
        remaining = self.max_runs - self.runs
        if remaining < 1:
            self.complete = False
            return None
        base = self.cur

        def interesting(sub: list[Any]) -> bool:
            self.runs += 1
            candidate = build(base, sub)
            return candidate is not None and self.ok(candidate)

        try:
            kept, _used, done = ddmin(units, interesting, max_runs=remaining)
        except NotInterestingError:
            self.complete = False
            return None
        if not done:
            self.complete = False
        return kept

    def phase_setup(self) -> None:
        setup = self.cur.get("setup")
        if not isinstance(setup, str) or not setup.strip():
            return
        stmts = _texts(setup)
        if not stmts:
            return

        def build(base: dict[str, Any], sub: list[str]) -> Optional[dict[str, Any]]:
            cand = copy.deepcopy(base)
            if sub:
                cand["setup"] = _join(sub)
            else:
                cand.pop("setup", None)
            return cand

        kept = self.reduce(stmts, build)
        if kept is not None and len(kept) < len(stmts):
            self.cur = build(self.cur, kept) or self.cur

    def phase_steps(self) -> None:
        groups = _step_groups(self.cur)

        def build(base: dict[str, Any], sub: list[list[int]]) -> Optional[dict[str, Any]]:
            keep = sorted(i for group in sub for i in group)
            if not keep:
                return None
            cand = copy.deepcopy(base)
            cand["steps"] = [copy.deepcopy(base["steps"][i]) for i in keep]
            return cand

        kept = self.reduce(groups, build)
        if kept is not None and sum(len(g) for g in kept) < len(self.cur.get("steps") or []):
            self.cur = build(self.cur, kept) or self.cur

    def phase_statements(self) -> None:
        k = 0
        while k < len(self.cur.get("steps") or []):
            step = self.cur["steps"][k]
            sql = step.get("sql") if isinstance(step, dict) else None
            if isinstance(sql, str):
                stmts = _texts(sql)
                if len(stmts) > 1:
                    def build(base: dict[str, Any], sub: list[str],
                              index: int = k) -> Optional[dict[str, Any]]:
                        if not sub:
                            return None
                        cand = copy.deepcopy(base)
                        cand["steps"][index]["sql"] = _join(sub)
                        return cand

                    kept = self.reduce(stmts, build)
                    if kept is not None and len(kept) < len(stmts):
                        self.cur = build(self.cur, kept) or self.cur
            k += 1

    def run(self) -> None:
        while True:
            before = json.dumps(self.cur, sort_keys=True, default=str)
            self.phase_setup()
            self.phase_steps()
            self.phase_statements()
            if not self.complete:
                return
            if json.dumps(self.cur, sort_keys=True, default=str) == before:
                return


# ---------------------------------------------------------------------------------------------
# Public API
# ---------------------------------------------------------------------------------------------


def minimize_script(script: str, interesting: Callable[[str], bool],
                    max_runs: int = 400) -> MinimizeResult:
    """Statement-level minimization of a SQL script."""
    stmts = _texts(script)

    def join_units(sub: list[str]) -> str:
        return _join(sub) if sub else ""

    kept, runs, complete = ddmin(stmts, lambda sub: interesting(join_units(sub)),
                                 max_runs=max_runs)
    return MinimizeResult(units_before=len(stmts), units_after=len(kept), runs=runs,
                          complete=complete, case_dict={}, script=join_units(kept))


def minimize_case(case_dict: dict[str, Any], same_failure: Callable[[dict[str, Any]], bool],
                  max_runs: int = 400) -> MinimizeResult:
    """Phased minimization of a case dict (module docstring)."""
    if max_runs < 1:
        raise ValueError("max_runs must be at least 1")
    minimizer = _Minimizer(case_dict, same_failure, max_runs)
    minimizer.runs += 1
    if not minimizer.ok(minimizer.cur):
        raise NotInterestingError("original input is not interesting")
    minimizer.run()

    out = copy.deepcopy(minimizer.cur)
    orig_id = str(case_dict.get("id", ""))
    family = str(case_dict.get("family", ""))
    if family and orig_id.startswith(family + "."):
        name = orig_id[len(family) + 1:]
    else:
        name = orig_id.split(".", 1)[-1]
    out["id"] = f"{family}.min-{name}"
    out["provenance"] = {"source": "minimized", "ref": orig_id}
    return MinimizeResult(units_before=_count_units(case_dict), units_after=_count_units(out),
                          runs=minimizer.runs, complete=minimizer.complete, case_dict=out)


def _has_harness_failure(transcript: Any) -> bool:
    """True if any step of the transcript hit a harness error or a timeout."""
    for obs in list(transcript.setup) + list(transcript.steps):
        if obs.harness_error is not None or obs.timed_out:
            return True
    return False


def same_failure_predicate(original: Any, reference_target: Any, target: Any) -> \
        Callable[[dict[str, Any]], bool]:
    """Build the case predicate: ``original`` is the model.CaseResult of the failing case;
    ``reference_target`` / ``target`` are runner.targets.Target objects.

    The predicate returns False (not interesting) for a candidate that fails to build, to
    execute, or to run on the reference (harness error or timeout). Exceptions raised while
    executing a candidate are treated the same way."""

    def predicate(candidate: dict[str, Any]) -> bool:
        from ..cases.model import Case
        from ..compare.classify import classify
        from ..compare.typed import compare_transcripts
        from ..runner.executor import execute_case

        try:
            case = Case.from_dict(candidate, path=_rel_path(candidate))
            reference = execute_case(case, reference_target, "reference")
            if _has_harness_failure(reference):
                return False
            observed = execute_case(case, target, "target")
            diffs = compare_transcripts(case, reference, observed)
            result = classify(case, reference, observed, diffs)
        except Exception:  # noqa: BLE001 — a candidate that cannot run is not interesting
            return False
        if result.outcome != original.outcome:
            return False
        return all(result.signature.get(f, "") == original.signature.get(f, "")
                   for f in _SIGNATURE_FIELDS)

    return predicate
