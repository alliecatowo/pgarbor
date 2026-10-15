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

B. Comparator mutants (simulate a broken verifier): wrappers around compare.typed functions
   that weaken comparison (``ignore_type_oids``, ``ignore_nulls`` (None == ""),
   ``always_multiset``, ``ignore_sqlstate``, ``ignore_command_tags``). For each comparator
   mutant there is at least one observation mutant from A that the REAL comparator flags and
   the WEAKENED one passes — the selftest asserts this "kill" relation, proving the fixture
   corpus is strong enough to notice a verifier regression.
"""

from __future__ import annotations

from typing import Callable, Optional

from ..model import Outcome, Transcript

ObservationMutant = Callable[[Transcript], Optional[Transcript]]

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


def observation_mutants() -> dict[str, ObservationMutant]:
    """name -> mutant function for every key of EXPECTED_OUTCOME."""
    raise NotImplementedError("HAR-27: observation mutants")


def comparator_mutant(name: str) -> Callable[..., list]:
    """Return a weakened ``compare_transcripts(case, ref, tgt) -> list[Diff]`` for a
    COMPARATOR_KILLS key (implemented by post-filtering / pre-mutating the real comparator's
    inputs, never by editing compare.typed)."""
    raise NotImplementedError("HAR-27: comparator mutants")
