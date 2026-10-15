"""ORACLE-05: compare a regress results file with expected outputs exactly like pg_regress.

Lease HAR-18.

pg_regress.c results_differ() semantics (reference reading):
  1. candidate expected files: the resultmap substitute if any, else ``expected/<t>.out``;
     then ``expected/<t>_0.out`` .. ``expected/<t>_9.out`` that exist;
  2. a test PASSES if the results file is identical to ANY candidate, compared byte-exactly
     after ``--strip-trailing-cr`` (a ``\\r`` immediately before ``\\n`` is removed on both
     sides; nothing else is normalized: whitespace, trailing newline, case all matter);
  3. on failure, the reported diff is against the candidate with the FEWEST differing lines
     (count of '+'/'-' lines in a unified diff, ties -> earliest candidate), rendered as a
     unified diff with 3 lines of context (``diff -U3``), headers
     ``diff -U3 <expected path> <results path>`` / ``--- <expected path>\\t<mtime>`` style is
     NOT required: use ``--- <expected>`` and ``+++ <results>`` without timestamps (difflib).

Also used for the differential baseline: comparing a target's results file with the PG
oracle's recorded results file (``tests/expected/regress/<t>.out``) uses the same function
with a single candidate.
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Optional, Sequence


@dataclass
class OutDiff:
    identical: bool
    matched: Optional[Path]        #: candidate that matched (when identical)
    best: Optional[Path]           #: closest candidate (when not identical)
    diff_lines: int                #: +/- line count vs best (0 when identical)
    unified: str                   #: unified diff text vs best ("" when identical)


def strip_trailing_cr(data: bytes) -> bytes:
    """Remove a CR immediately preceding LF (implemented)."""
    return data.replace(b"\r\n", b"\n")


def compare_output(results: Path, candidates: Sequence[Path]) -> OutDiff:
    """Apply the algorithm above. No candidates -> OutDiff(False, None, None, -1,
    "no expected output") ; missing results file -> same with "no results file"."""
    raise NotImplementedError("HAR-18: pg_regress output comparison")


def unified_diff(expected: Path, results: Path, context: int = 3) -> tuple[int, str]:
    """(changed line count, unified diff text) using difflib on lines decoded UTF-8 with
    errors='surrogateescape' and keepends=True."""
    raise NotImplementedError("HAR-18: unified diff")
