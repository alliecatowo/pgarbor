"""ORACLE-13: known-bad mutations the verifier MUST detect.

Lease HAR-27.

Two mutant families:

A. Observation mutants (simulate a broken engine): functions ``Transcript -> Transcript``
   (deep copy, then mutate the FIRST applicable location; return None when not applicable).
   Each must make compare.typed + compare.classify produce a non-PASS outcome against the
   unmutated reference:
     ``change_value``        first non-NULL text value: append "x"           -> MISMATCH
     ``null_to_empty``       first NULL value -> ""                          -> MISMATCH
     ``empty_to_null``       first "" value -> None                          -> MISMATCH
     ``change_type_oid``     first column type_oid 23 -> 20 (else +1)        -> MISMATCH
     ``change_typmod``       first column with typmod != -1: typmod + 1      -> MISMATCH
     ``change_sqlstate``     first error sqlstate -> "42000" (if it already is "42000",
                             -> "42601")                                   -> MISMATCH
     ``swallow_error``       first error result -> kind "command" tag "SELECT 0"
                                                          -> UNEXPECTED_ACCEPTANCE
     ``unsupported``         first non-error result -> error 0A000 "not implemented"
                                                          -> UNSUPPORTED
     ``drop_duplicate_row``  first rows result having a duplicate row: remove one copy
                                                          -> MISMATCH (multiset must keep dups)
     ``drop_last_row``       first rows result with rows: remove last row      -> MISMATCH
     ``reorder_ordered``     first ORDERED rows result with >= 2 distinct rows: reverse
                                                          -> MISMATCH (row_order)
     ``change_command_tag``  first command tag "INSERT 0 n" -> "INSERT 0 n+1" (else append
                             " 0")                         -> MISMATCH
     ``change_txn_status``   first step txn_status "T" -> "I" (else "I" -> "T") -> MISMATCH
     ``drop_notice``         first step with notices: remove the first notice  -> MISMATCH
     ``crash_midway``        last step: connection_lost=True, results=[]       -> CRASH
     ``hang_midway``         last step: timed_out=True, results=[]             -> HANG
     ``corrupt``             first result -> error XX001 "invalid page"        -> CORRUPTION
   ``reorder_unordered`` (reverse rows of a MULTISET result) is the one *negative* control:
   it MUST still PASS (proves multiset comparison is used where PG order is unspecified).

   The two reorder mutants need the case to know which rows results are ORDERED or
   MULTISET (``compare.typed.resolve_order``). Every observation mutant therefore has the
   signature ``(transcript, case=None)``; the reorder mutants return None when ``case`` is
   not given. Selftest callers must pass the case.

B. Comparator mutants (simulate a broken verifier): wrappers around compare.typed functions
   that weaken comparison (``ignore_type_oids``, ``ignore_nulls`` (None == ""),
   ``always_multiset``, ``ignore_sqlstate``, ``ignore_command_tags``). For each comparator
   mutant there is at least one observation mutant from A that the REAL comparator flags and
   the WEAKENED one passes — the selftest asserts this "kill" relation, proving the fixture
   corpus is strong enough to notice a verifier regression.
"""

from __future__ import annotations

import copy
import re
from typing import Callable, Optional

from ..cases.model import Case
from ..compare import typed as _typed
from ..model import Diff, ErrorInfo, Outcome, StatementResult, StepObservation, Transcript

ObservationMutant = Callable[..., Optional[Transcript]]

#: name -> expected outcome when applied (CONTRACT for selftest.run).
EXPECTED_OUTCOME: dict[str, Outcome] = {
    "change_value": Outcome.MISMATCH,
    "null_to_empty": Outcome.MISMATCH,
    "empty_to_null": Outcome.MISMATCH,
    "change_type_oid": Outcome.MISMATCH,
    "change_typmod": Outcome.MISMATCH,
    "change_sqlstate": Outcome.MISMATCH,
    "swallow_error": Outcome.UNEXPECTED_ACCEPTANCE,
    "unsupported": Outcome.UNSUPPORTED,
    "drop_duplicate_row": Outcome.MISMATCH,
    "drop_last_row": Outcome.MISMATCH,
    "reorder_ordered": Outcome.MISMATCH,
    "change_command_tag": Outcome.MISMATCH,
    "change_txn_status": Outcome.MISMATCH,
    "drop_notice": Outcome.MISMATCH,
    "crash_midway": Outcome.CRASH,
    "hang_midway": Outcome.HANG,
    "corrupt": Outcome.CORRUPTION,
    "reorder_unordered": Outcome.PASS,
}

#: comparator mutant name -> observation mutants it must fail to detect (kill relation).
COMPARATOR_KILLS: dict[str, tuple[str, ...]] = {
    "ignore_type_oids": ("change_type_oid",),
    "ignore_nulls": ("null_to_empty", "empty_to_null"),
    "always_multiset": ("reorder_ordered",),
    "ignore_sqlstate": ("change_sqlstate",),
    "ignore_command_tags": ("change_command_tag",),
}

_INSERT_TAG = re.compile(r"INSERT 0 (\d+)")


# ---------------------------------------------------------------------------------------------
# Helpers (private)
# ---------------------------------------------------------------------------------------------


def _all_steps(t: Transcript) -> list[StepObservation]:
    """Setup observations first, then case steps: the transcript's own order."""
    return [*t.setup, *t.steps]


def _error(sqlstate: str, message: str) -> ErrorInfo:
    return ErrorInfo(severity="ERROR", sqlstate=sqlstate, message=message)


def _clear_result(r: StatementResult) -> None:
    """Drop every payload field of a result (keeps ``kind`` and ``error`` for the caller)."""
    r.columns = []
    r.rows = []
    r.command_tag = None
    r.param_type_oids = None


# ---------------------------------------------------------------------------------------------
# A. Observation mutants
# ---------------------------------------------------------------------------------------------


def _change_value(t: Transcript, case: Optional[Case] = None) -> Optional[Transcript]:
    m = copy.deepcopy(t)
    for s in _all_steps(m):
        for r in s.results:
            for row in r.rows:
                for c, v in enumerate(row):
                    if isinstance(v, str):
                        row[c] = v + "x"
                        return m
    return None


def _null_to_empty(t: Transcript, case: Optional[Case] = None) -> Optional[Transcript]:
    m = copy.deepcopy(t)
    for s in _all_steps(m):
        for r in s.results:
            for row in r.rows:
                for c, v in enumerate(row):
                    if v is None:
                        row[c] = ""
                        return m
    return None


def _empty_to_null(t: Transcript, case: Optional[Case] = None) -> Optional[Transcript]:
    m = copy.deepcopy(t)
    for s in _all_steps(m):
        for r in s.results:
            for row in r.rows:
                for c, v in enumerate(row):
                    if isinstance(v, str) and v == "":
                        row[c] = None
                        return m
    return None


def _change_type_oid(t: Transcript, case: Optional[Case] = None) -> Optional[Transcript]:
    m = copy.deepcopy(t)
    for s in _all_steps(m):
        for r in s.results:
            for col in r.columns:
                col.type_oid = 20 if col.type_oid == 23 else col.type_oid + 1
                return m
    return None


def _change_typmod(t: Transcript, case: Optional[Case] = None) -> Optional[Transcript]:
    m = copy.deepcopy(t)
    for s in _all_steps(m):
        for r in s.results:
            for col in r.columns:
                if col.typmod != -1:
                    col.typmod += 1
                    return m
    return None


def _change_sqlstate(t: Transcript, case: Optional[Case] = None) -> Optional[Transcript]:
    m = copy.deepcopy(t)
    for s in _all_steps(m):
        for r in s.results:
            if r.error is not None:
                r.error.sqlstate = "42601" if r.error.sqlstate == "42000" else "42000"
                return m
    return None


def _swallow_error(t: Transcript, case: Optional[Case] = None) -> Optional[Transcript]:
    m = copy.deepcopy(t)
    for s in _all_steps(m):
        for r in s.results:
            if r.error is not None:
                r.kind = "command"
                r.command_tag = "SELECT 0"
                r.error = None
                return m
    return None


def _unsupported(t: Transcript, case: Optional[Case] = None) -> Optional[Transcript]:
    m = copy.deepcopy(t)
    for s in _all_steps(m):
        for r in s.results:
            if r.error is None:
                r.kind = "error"
                _clear_result(r)
                r.error = _error("0A000", "not implemented")
                return m
    return None


def _drop_duplicate_row(t: Transcript, case: Optional[Case] = None) -> Optional[Transcript]:
    m = copy.deepcopy(t)
    for s in _all_steps(m):
        for r in s.results:
            if r.kind != "rows":
                continue
            seen: set[tuple] = set()
            for i, row in enumerate(r.rows):
                key = tuple(row)
                if key in seen:
                    del r.rows[i]
                    return m
                seen.add(key)
    return None


def _drop_last_row(t: Transcript, case: Optional[Case] = None) -> Optional[Transcript]:
    m = copy.deepcopy(t)
    for s in _all_steps(m):
        for r in s.results:
            if r.kind == "rows" and r.rows:
                r.rows.pop()
                return m
    return None


def _reorder_rows_where(t: Transcript, case: Optional[Case], wanted: str) -> Optional[Transcript]:
    """Reverse the rows of the first ``rows`` result whose resolved order mode is ``wanted``
    and whose reversal actually changes the row sequence (so it is observable as a
    difference in ordered mode and is not a palindrome). Only case steps carry SQL, so setup
    observations are never reordered."""
    if case is None:
        return None
    m = copy.deepcopy(t)
    for s in m.steps:
        n = len(s.results)
        for k, r in enumerate(s.results):
            if r.kind != "rows" or len(r.rows) < 2:
                continue
            if r.rows[::-1] == r.rows:
                continue
            if _typed.resolve_order(case, s.index, k, n) != wanted:
                continue
            r.rows.reverse()
            return m
    return None


def _reorder_ordered(t: Transcript, case: Optional[Case] = None) -> Optional[Transcript]:
    return _reorder_rows_where(t, case, "ordered")


def _reorder_unordered(t: Transcript, case: Optional[Case] = None) -> Optional[Transcript]:
    return _reorder_rows_where(t, case, "multiset")


def _change_command_tag(t: Transcript, case: Optional[Case] = None) -> Optional[Transcript]:
    m = copy.deepcopy(t)
    results = [r for s in _all_steps(m) for r in s.results if r.command_tag is not None]
    for r in results:
        match = _INSERT_TAG.fullmatch(r.command_tag or "")
        if match is not None:
            r.command_tag = f"INSERT 0 {int(match.group(1)) + 1}"
            return m
    if results:
        results[0].command_tag = f"{results[0].command_tag} 0"
        return m
    return None


def _change_txn_status(t: Transcript, case: Optional[Case] = None) -> Optional[Transcript]:
    m = copy.deepcopy(t)
    steps = _all_steps(m)
    for s in steps:
        if s.txn_status == "T":
            s.txn_status = "I"
            return m
    for s in steps:
        if s.txn_status == "I":
            s.txn_status = "T"
            return m
    return None


def _drop_notice(t: Transcript, case: Optional[Case] = None) -> Optional[Transcript]:
    m = copy.deepcopy(t)
    for s in _all_steps(m):
        if s.notices:
            del s.notices[0]
            return m
    return None


def _crash_midway(t: Transcript, case: Optional[Case] = None) -> Optional[Transcript]:
    if not t.steps:
        return None
    m = copy.deepcopy(t)
    last = m.steps[-1]
    last.connection_lost = True
    last.results = []
    return m


def _hang_midway(t: Transcript, case: Optional[Case] = None) -> Optional[Transcript]:
    if not t.steps:
        return None
    m = copy.deepcopy(t)
    last = m.steps[-1]
    last.timed_out = True
    last.results = []
    return m


def _corrupt(t: Transcript, case: Optional[Case] = None) -> Optional[Transcript]:
    m = copy.deepcopy(t)
    for s in _all_steps(m):
        if s.results:
            r = s.results[0]
            r.kind = "error"
            _clear_result(r)
            r.error = _error("XX001", "invalid page")
            return m
    return None


_OBSERVATION: dict[str, ObservationMutant] = {
    "change_value": _change_value,
    "null_to_empty": _null_to_empty,
    "empty_to_null": _empty_to_null,
    "change_type_oid": _change_type_oid,
    "change_typmod": _change_typmod,
    "change_sqlstate": _change_sqlstate,
    "swallow_error": _swallow_error,
    "unsupported": _unsupported,
    "drop_duplicate_row": _drop_duplicate_row,
    "drop_last_row": _drop_last_row,
    "reorder_ordered": _reorder_ordered,
    "change_command_tag": _change_command_tag,
    "change_txn_status": _change_txn_status,
    "drop_notice": _drop_notice,
    "crash_midway": _crash_midway,
    "hang_midway": _hang_midway,
    "corrupt": _corrupt,
    "reorder_unordered": _reorder_unordered,
}


# ---------------------------------------------------------------------------------------------
# B. Comparator mutants: pre-mutate both sides, then run the REAL comparator
# ---------------------------------------------------------------------------------------------

PreMutate = Callable[[Case, Transcript, Transcript], tuple[Case, Transcript, Transcript]]


def _pre_ignore_type_oids(case: Case, ref: Transcript, tgt: Transcript):
    for t in (ref, tgt):
        for s in _all_steps(t):
            for r in s.results:
                for col in r.columns:
                    col.type_oid = 0
                if r.param_type_oids is not None:
                    r.param_type_oids = [0 for _ in r.param_type_oids]
    return case, ref, tgt


def _pre_ignore_nulls(case: Case, ref: Transcript, tgt: Transcript):
    for t in (ref, tgt):
        for s in _all_steps(t):
            for r in s.results:
                for row in r.rows:
                    for c, v in enumerate(row):
                        if v is None:
                            row[c] = ""
    return case, ref, tgt


def _pre_always_multiset(case: Case, ref: Transcript, tgt: Transcript):
    weak = copy.deepcopy(case)
    for st in weak.steps:
        st.order = "multiset"
    return weak, ref, tgt


def _pre_ignore_sqlstate(case: Case, ref: Transcript, tgt: Transcript):
    for t in (ref, tgt):
        for s in _all_steps(t):
            for r in s.results:
                if r.error is not None:
                    r.error.sqlstate = ""
    return case, ref, tgt


def _pre_ignore_command_tags(case: Case, ref: Transcript, tgt: Transcript):
    for t in (ref, tgt):
        for s in _all_steps(t):
            for r in s.results:
                r.command_tag = None
    return case, ref, tgt


_COMPARATOR_PRE: dict[str, PreMutate] = {
    "ignore_type_oids": _pre_ignore_type_oids,
    "ignore_nulls": _pre_ignore_nulls,
    "always_multiset": _pre_always_multiset,
    "ignore_sqlstate": _pre_ignore_sqlstate,
    "ignore_command_tags": _pre_ignore_command_tags,
}


# ---------------------------------------------------------------------------------------------
# Public API (CONTRACT)
# ---------------------------------------------------------------------------------------------


def observation_mutants() -> dict[str, ObservationMutant]:
    """name -> mutant function for every key of EXPECTED_OUTCOME."""
    return dict(_OBSERVATION)


def comparator_mutant(name: str) -> Callable[..., list]:
    """Return a weakened ``compare_transcripts(case, ref, tgt) -> list[Diff]`` for a
    COMPARATOR_KILLS key (implemented by post-filtering / pre-mutating the real comparator's
    inputs, never by editing compare.typed)."""
    pre = _COMPARATOR_PRE.get(name)
    if pre is None:
        raise KeyError(f"unknown comparator mutant {name!r}; expected one of "
                       f"{sorted(_COMPARATOR_PRE)}")

    def weakened(case: Case, reference: Transcript, target: Transcript) -> list[Diff]:
        c, r, t = pre(case, copy.deepcopy(reference), copy.deepcopy(target))
        return _typed.compare_transcripts(c, r, t)

    weakened.__name__ = f"compare_{name}"
    return weakened
